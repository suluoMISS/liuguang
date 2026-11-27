//! `--self-test`：不开窗口，只用 IPC 把 GUI 依赖的每条命令走一遍。
//!
//! 为什么需要它：GUI 的价值全在"能不能正确地下命令、正确地读回状态"，
//! 而这一步在无人值守的 CI / 远程机器上没法靠点鼠标验证。
//! 把同一条 `ipc::Cmd` 序列跑一遍并逐项打印 PASS/FAIL，
//! 就能在**没有窗口**的环境里证明"GUI 的 IPC 层是好的"。
//!
//! 它复用的是窗口用的同一个 `ipc::spawn`，所以不存在"测的和用的不是一条路"。
//!
//! 覆盖面（0.3.0 起）：状态/媒体/指标/应用/暂停/继续/重载/输出/缩略图/转场/pin/重扫，
//! 外加播放列表 CRUD、收藏偏好往返、设置读写——以及**结束时把用户现场原样恢复**。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use liuguang_core::config::Config;
use liuguang_ipc::protocol::{CompatState, MediaItem, Playlist, PrepareState};

use crate::details;
use crate::ipc::{self, Cmd, PlaylistEdit, Upd};
use crate::state::Prefs;
use crate::ui;

/// 单项等待上限：daemon 正常时都在毫秒级，超过这个数就是真卡住了。
const WAIT: Duration = Duration::from_secs(10);
/// 抽视频封面要起一条解码管线（冷启动 ~0.6s，4K 更久），给宽裕一点。
const THUMB_WAIT: Duration = Duration::from_secs(20);
/// 真转码（8K/10bit 素材）可能要几分钟；只等“ready”，不拿“已受理”糊弄。
const COMPAT_WAIT: Duration = Duration::from_secs(180);

/// 自检开始前记下的现场，结束时原样恢复。
///
/// 为什么必须恢复：`--self-test` 是对**正在用的**守护进程下命令，`apply` 那一步会真的
/// 换掉用户当前壁纸。第一版没恢复，跑一次自检就把人家钉住的视频换成了列表第一张图
/// （还顺手把 `pinned` 清零）——自检不该有这种副作用。
struct Snapshot {
    output: Option<String>,
    media: Option<String>,
    pinned: bool,
    paused: bool,
    setting: RenderSetting,
}

/// 一个输出的适配参数（恢复现场用）。
#[derive(Clone)]
struct RenderSetting {
    fit: String,
    zoom: f64,
    offset_x: f64,
    offset_y: f64,
    /// 0 / 90 / 180 / 270。
    rotation: u32,
}

impl Default for RenderSetting {
    fn default() -> Self {
        Self {
            fit: "cover".to_owned(),
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: 0,
        }
    }
}

/// 运行自检，返回进程退出码（0 = 全过）。
///
/// `config_path` 是**解析后**的配置路径（`--config` 优先于 persona 默认），
/// 与 GUI 运行期读配置、恢复 fit/zoom/offset/rotation 用的是同一份文件。
pub fn run(socket: &Path, config_path: &Path) -> i32 {
    println!(
        "liuguang-gui --self-test：socket={} config={}",
        socket.display(),
        config_path.display()
    );
    let (tx, _urgent, rx) = ipc::spawn(socket.to_path_buf(), config_path.to_path_buf());
    let mut failures = 0usize;
    let mut checks = 0usize;
    let mut check = |name: &str, ok: bool, detail: &str| {
        println!("{} {name} —— {detail}", if ok { "PASS" } else { "FAIL" });
        checks += 1;
        if !ok {
            failures += 1;
        }
    };

    // 1) status：能连上、能解析出状态（断连时这一条就会先红）。
    let _ = tx.send(Cmd::Status);
    let mut snapshot: Option<Snapshot> = None;
    match next(&rx, WAIT, |u| matches!(u, Upd::Status(_) | Upd::Error(_))) {
        Some(Upd::Status(status)) => {
            // 记下现场：后面的 apply / pause 会真的改动它，结束时按这里恢复。
            if let Some(channel) = status.channels.first() {
                snapshot = Some(Snapshot {
                    output: Some(channel.output.clone()),
                    media: channel.media.clone(),
                    pinned: channel.pinned,
                    paused: channel.paused,
                    setting: restore_settings(config_path, &channel.output),
                });
            }
            check(
                "status",
                true,
                &format!(
                    "state={} output={}",
                    status.state,
                    status.output.as_deref().unwrap_or("-")
                ),
            );
        }
        Some(Upd::Error(message)) => check("status", false, &message),
        _ => check("status", false, "超时：守护进程没有回应 status"),
    }

    // 2) media：列表能拉出来（扫描中会自己再轮询几轮）。
    let media = wait_media(&tx, &rx);
    match &media {
        Ok(rows) => check(
            "media",
            true,
            &format!("{} 项（{}）", rows.len(), sample(rows)),
        ),
        Err(message) => check("media", false, message),
    }

    // 3) metrics：CPU / 内存 / fps（底部状态条的数据源）。
    let _ = tx.send(Cmd::Metrics);
    match next(&rx, WAIT, |u| matches!(u, Upd::Metrics(_) | Upd::Error(_))) {
        Some(Upd::Metrics(metrics)) => check(
            "metrics",
            metrics.fps >= 0.0,
            &format!(
                "cpu={:.1}% mem={} fps={:.2} channels={}",
                metrics.cpu_percent,
                crate::ui::human_size(metrics.mem_bytes),
                metrics.fps,
                metrics.channels.len()
            ),
        ),
        Some(Upd::Error(message)) => check("metrics", false, &message),
        _ => check("metrics", false, "超时：没有收到指标"),
    }

    let setting = snapshot
        .as_ref()
        .map_or_else(RenderSetting::default, |s| s.setting.clone());

    // 4) apply：挑一个**能播**的条目应用，等受理。
    //
    // 以前直接取 `rows.first()`：库里首条若是 8K H.264 / 10bit（需要转码），
    // apply 必然被 `NEEDS_PREPARATION` 拦下，自检就红了——红的是挑选逻辑，不是应用链路。
    // 现在优先挑 `compat == playable` 的素材；一条都没有时，先把它准备到 ready 再应用。
    match pick_apply_item(&tx, &rx, media.as_ref().ok()) {
        Some(item) => {
            let _ = tx.send(Cmd::Apply(Box::new(ipc::ApplyRequest {
                path: item.path.clone(),
                output: None,
                fit: setting.fit.clone(),
                zoom: setting.zoom,
                offset_x: setting.offset_x,
                offset_y: setting.offset_y,
                rotation: setting.rotation,
                brightness: 1.0,
                pin: false,
            })));
            match next(&rx, WAIT, |u| matches!(u, Upd::Applied(_) | Upd::Error(_))) {
                Some(Upd::Applied(path)) => check("apply", true, &path),
                Some(Upd::Error(message)) => check("apply", false, &message),
                _ => check("apply", false, "超时：没有收到受理回执"),
            }
        }
        None => check("apply", false, "媒体库为空，无法验证应用链路"),
    }

    // 5) pause / resume：状态要如实翻转。
    let _ = tx.send(Cmd::Pause);
    match next(&rx, WAIT, |u| matches!(u, Upd::Paused(_) | Upd::Error(_))) {
        Some(Upd::Paused(true)) => check("pause", true, "paused=true"),
        Some(Upd::Paused(false)) => check("pause", false, "daemon 回了 paused=false"),
        Some(Upd::Error(message)) => check("pause", false, &message),
        _ => check("pause", false, "超时"),
    }
    let _ = tx.send(Cmd::Resume);
    match next(&rx, WAIT, |u| matches!(u, Upd::Paused(_) | Upd::Error(_))) {
        Some(Upd::Paused(false)) => check("resume", true, "paused=false"),
        Some(Upd::Paused(true)) => check("resume", false, "daemon 回了 paused=true"),
        Some(Upd::Error(message)) => check("resume", false, &message),
        _ => check("resume", false, "超时"),
    }

    // 6) reload：配置重载不报错。
    let _ = tx.send(Cmd::Reload);
    match next(&rx, WAIT, |u| matches!(u, Upd::Reloaded | Upd::Error(_))) {
        Some(Upd::Reloaded) => check("reload", true, "配置已重载"),
        Some(Upd::Error(message)) => check("reload", false, &message),
        _ => check("reload", false, "超时"),
    }

    // 7) outputs：输出列表能拉到（多显示器选择的数据源）。
    let _ = tx.send(Cmd::Outputs);
    match next(&rx, WAIT, |u| matches!(u, Upd::Outputs(_) | Upd::Error(_))) {
        Some(Upd::Outputs(list)) => {
            let names: Vec<String> = list
                .iter()
                .map(|output| {
                    output.mode.as_ref().map_or_else(
                        || output.name.clone(),
                        |mode| format!("{} ({mode})", output.name),
                    )
                })
                .collect();
            check(
                "outputs",
                !names.is_empty(),
                &format!("{} 个（{}）", names.len(), names.join(", ")),
            );
        }
        Some(Upd::Error(message)) => check("outputs", false, &message),
        _ => check("outputs", false, "超时"),
    }

    // 8) thumbnail：优先拿视频（这条链路只有守护进程能做），没有视频就退图片。
    //    顺带覆盖 t4 新增的 `at_ms` 参数：取第 2 秒那一帧。
    let video = media.as_ref().ok().and_then(|rows| {
        rows.iter()
            .find(|row| row.kind != "image")
            .or_else(|| rows.first())
            .cloned()
    });
    let video_kind = video
        .as_ref()
        .map(|row| row.kind.clone())
        .unwrap_or_default();
    match video {
        Some(item) => {
            // 图片忽略 at_ms；视频必须真的按时间取帧——悬停动效全部依赖这条链路，
            // 所以 at_ms>0 失败就是 FAIL，不允许静默回退到首帧把问题藏过去。
            let at_ms = if video_kind == "image" { 0 } else { 2000 };
            let _ = tx.send(Cmd::Thumbnail {
                id: item.id.clone(),
                at_ms,
                max_width: 400,
                max_height: 250,
            });
            match next(&rx, THUMB_WAIT, |u| {
                matches!(u, Upd::Thumbnail { .. } | Upd::Error(_))
            }) {
                Some(Upd::Thumbnail {
                    png_b64,
                    id,
                    at_ms: returned_at,
                }) => {
                    let ok = !png_b64.is_empty() && returned_at == at_ms && id == item.id;
                    // 视频还要求"第 2 秒"真的不是首帧：只验 ok 会让 seek 失效也蒙混过关
                    // （与 `scripts/e2e/probe_thumbnail_at_ms.py` 的像素判据同一思路）。
                    let mut detail = format!(
                        "{}（{video_kind}）· at_ms={returned_at} · base64 {} 字节",
                        short(&item.path),
                        png_b64.len()
                    );
                    let mut ok = ok;
                    if ok && at_ms > 0 {
                        let _ = tx.send(Cmd::Thumbnail {
                            id: item.id.clone(),
                            at_ms: 0,
                            max_width: 400,
                            max_height: 250,
                        });
                        match next(&rx, THUMB_WAIT, |u| {
                            matches!(u, Upd::Thumbnail { .. } | Upd::Error(_))
                        }) {
                            Some(Upd::Thumbnail { png_b64: first, .. }) if first == png_b64 => {
                                ok = false;
                                detail.push_str("；与首帧字节相同（seek 未生效）");
                            }
                            Some(Upd::Thumbnail { .. }) => detail.push_str("；与首帧不同"),
                            Some(Upd::Error(message)) => {
                                ok = false;
                                detail.push_str(&format!("；首帧对照失败：{message}"));
                            }
                            _ => {
                                ok = false;
                                detail.push_str("；首帧对照超时");
                            }
                        }
                    }
                    check("thumbnail", ok, &detail);
                }
                Some(Upd::Error(message)) => check(
                    "thumbnail",
                    false,
                    &format!("at_ms={at_ms} 取帧失败（悬停动效依赖这条链路）：{message}"),
                ),
                _ => check("thumbnail", false, "超时"),
            }
        }
        None => println!("SKIP thumbnail —— 媒体库为空，没有可抽帧的文件"),
    }

    // 9) transition：设置转场（改运行时，不落盘）。
    let _ = tx.send(Cmd::Transition {
        effect: "fade".to_owned(),
        duration_ms: 500,
    });
    match next(&rx, WAIT, |u| {
        matches!(u, Upd::Transition(_) | Upd::Error(_))
    }) {
        Some(Upd::Transition(effect)) => check("transition", effect == "fade", &effect),
        Some(Upd::Error(message)) => check("transition", false, &message),
        _ => check("transition", false, "超时"),
    }

    // 10) pin：切到"只放这一项"再切回来（按输出锁定，用的是快照里的输出名）。
    let pin_output = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.output.clone());
    let _ = tx.send(Cmd::SetPinned {
        pinned: true,
        output: pin_output.clone(),
    });
    let mut pin_ok = true;
    match next(&rx, WAIT, |u| matches!(u, Upd::Pinned(_) | Upd::Error(_))) {
        Some(Upd::Pinned(true)) => {}
        Some(Upd::Pinned(false)) => pin_ok = false,
        Some(Upd::Error(message)) => {
            pin_ok = false;
            println!("  pin 错误：{message}");
        }
        _ => pin_ok = false,
    }
    let _ = tx.send(Cmd::SetPinned {
        pinned: false,
        output: pin_output,
    });
    let _ = next(&rx, WAIT, |u| matches!(u, Upd::Pinned(_) | Upd::Error(_)));
    check(
        "pin",
        pin_ok,
        if pin_ok {
            "已锁定单项播放并恢复轮换"
        } else {
            "锁定失败"
        },
    );

    // 11) rescan：让 daemon 真的重扫磁盘。
    let _ = tx.send(Cmd::Rescan);
    let _ = next(&rx, WAIT, |u| {
        matches!(u, Upd::Media { .. } | Upd::Error(_))
    });
    match wait_media(&tx, &rx) {
        Ok(rows) => check("rescan", true, &format!("重扫后 {} 项", rows.len())),
        Err(message) => check("rescan", false, &message),
    }

    // 12) 播放列表 CRUD：新建 → 改名 → 列表 → 删除（媒体的 IPC CRUD 版）。
    check_playlists(&tx, &rx, media.as_ref().ok(), &mut check);

    // 13) 收藏偏好往返（gui.json 的读写）。
    check_prefs(media.as_ref().ok(), &mut check);

    // 14) 设置读写（Config 的加载/修改/原子保存/重读）。
    check_settings(config_path, &mut check);

    // 15) 媒体来源：设置页的数据源。
    let _ = tx.send(Cmd::Sources);
    match next(&rx, WAIT, |u| matches!(u, Upd::Sources(_) | Upd::Error(_))) {
        Some(Upd::Sources(sources)) => check(
            "sources",
            !sources.root.is_empty(),
            &format!(
                "root={} · {} 个来源{}",
                sources.root,
                sources.sources.len(),
                if sources.missing == 0 {
                    String::new()
                } else {
                    format!(" · 缺失 {}", sources.missing)
                }
            ),
        ),
        Some(Upd::Error(message)) => check("sources", false, &message),
        _ => check("sources", false, "超时"),
    }

    // 16) 兼容性 / 转码：库里找一条 NeedsConversion 素材，验证角标数据与准备链路。
    check_compat(&tx, &rx, media.as_ref().ok(), &mut check);

    // 收尾：把现场恢复原样。
    //
    // 这一段不计入检查项——它不是"被测能力"，是自检自己的义务。
    if let Some(snapshot) = snapshot {
        restore(&tx, &rx, &snapshot);
    }

    println!(
        "{}",
        if failures == 0 {
            format!("全部通过（{checks} 项）")
        } else {
            format!("{failures} 项失败（共 {checks} 项）")
        }
    );
    if failures == 0 { 0 } else { 1 }
}

/// 兼容性 / 转码（t17 第 20 项）。
///
/// 判据：
/// * 找到 NeedsConversion 素材 ⇒ 角标必须能表示出来（`compat_badge` 有文案），
///   且 `prepare_media` 必须被受理（queued/running/ready）；
/// * 库里没有 NeedsConversion 素材，或后端没有 ffmpeg/转码被关闭 ⇒ **SKIP**
///   （不是 FAIL：这是环境缺能力，不是 GUI 缺陷）。
fn check_compat(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
    media: Option<&Vec<MediaItem>>,
    check: &mut impl FnMut(&str, bool, &str),
) {
    let Some(rows) = media else {
        println!("SKIP compat —— 媒体库为空");
        return;
    };
    let videos: Vec<&MediaItem> = rows.iter().filter(|item| item.kind == "video").collect();
    if videos.is_empty() {
        println!("SKIP compat —— 库里没有视频");
        return;
    }

    // 逐条查 get_media，找一条 NeedsConversion（10bit / 8K H.264 / 不可硬解 AV1）。
    let mut target: Option<(MediaItem, liuguang_ipc::protocol::CompatInfo)> = None;
    for item in videos {
        let _ = tx.send(Cmd::MediaCompat {
            id: item.id.clone(),
        });
        match next(rx, WAIT, |u| {
            matches!(u, Upd::Compat { .. } | Upd::Error(_))
        }) {
            Some(Upd::Compat {
                compat: Some(info), ..
            }) if info.state == CompatState::NeedsConversion => {
                target = Some((item.clone(), info));
                break;
            }
            Some(Upd::Compat { .. }) => {}
            Some(Upd::Error(message)) => {
                println!("SKIP compat —— 后端不支持 get_media：{message}");
                return;
            }
            _ => {
                println!("SKIP compat —— get_media 超时");
                return;
            }
        }
    }
    let Some((item, info)) = target else {
        println!("SKIP compat —— 库里没有 NeedsConversion 素材（放一条 10bit 或 8K H.264 再测）");
        return;
    };

    let badge = ui::compat_badge(&info);
    let badge_exact = badge == Some(("需转码", "warn"));
    let badge_text = badge.map_or_else(
        || "（没有角标）".to_owned(),
        |(text, class)| format!("{text}/{class}"),
    );
    let reason = info
        .reason
        .clone()
        .unwrap_or_else(|| "（无原因文本）".to_owned());
    let converted = info
        .target
        .as_deref()
        .map_or_else(|| "（无目标）".to_owned(), ui::target_label);
    let before = source_stamp(Path::new(&item.path));

    // 准备（转码）：必须真的等到 ready。
    //
    // t18 F2：以前把 `Queued`/`Running` 也算 PASS，而选中 NeedsConversion 后角标必然非空，
    // 于是这项判据恒真——自检永远绿，比没有更糟。现在只有 ready 才过，超时明确失败。
    //
    // t31 F1：完成通知有两条路（命令回包 `Upd::Prepared` 与事件 `MEDIA_PREPARED`），
    // 而冷缓存下第一次回包常常只是 `Queued`。旧实现发一次就等，永远等不到 ready。
    // 这里改成**每 500ms 重发一次幂等查询** `prepare_media(force=false)`：
    // daemon 对同 key 的在途任务只回状态、不会重复排队，所以它同时充当进度轮询。
    let _ = tx.send(Cmd::Prepare {
        path: item.path.clone(),
        force: false,
    });
    let deadline = Instant::now() + COMPAT_WAIT;
    let mut last_send = Instant::now();
    let mut last = "（还没收到状态）".to_owned();
    loop {
        if Instant::now() > deadline {
            check(
                "compat",
                false,
                &format!(
                    "{} · 等待 ready 超时（{}s，最后状态 {last}）",
                    item.name,
                    COMPAT_WAIT.as_secs()
                ),
            );
            return;
        }
        // 轮询（幂等）：同一条命令既是状态查询，也让"只发一次就卡住"不可能再发生。
        if last_send.elapsed() >= Duration::from_millis(500) {
            let _ = tx.send(Cmd::Prepare {
                path: item.path.clone(),
                force: false,
            });
            last_send = Instant::now();
        }
        match next(rx, Duration::from_millis(400), |u| {
            matches!(
                u,
                Upd::Prepared { .. } | Upd::Failure { .. } | Upd::Error(_)
            )
        }) {
            Some(Upd::Prepared { path, result }) if path == item.path => {
                last = format!("{:?} {}‰", result.state, result.progress_permille);
                match result.state {
                    PrepareState::Queued | PrepareState::Running => {}
                    PrepareState::Ready => {
                        let cache = result.cache_path.clone().unwrap_or_default();
                        let cache_ok = !cache.is_empty() && Path::new(&cache).exists();
                        let label = ui::prepare_label(
                            &PrepareState::Ready,
                            result.progress_permille,
                            info.target.as_deref(),
                        );
                        let label_ok = label.contains("已转换为") && label.contains(&converted);
                        let source_ok =
                            before.is_some() && source_stamp(Path::new(&item.path)) == before;
                        let ok = badge_exact && cache_ok && label_ok && source_ok;
                        check(
                            "compat",
                            ok,
                            &format!(
                                "{} · 角标 {badge_text} · 文案「{label}」 · cache={} · 源文件未改写={source_ok} · 原因 {reason}",
                                item.name,
                                if cache.is_empty() {
                                    "（缺失）"
                                } else {
                                    cache.as_str()
                                }
                            ),
                        );
                        return;
                    }
                    PrepareState::Failed => {
                        // t31 F4：`Failed` 是**结构化状态**（转码真的跑了但失败），
                        // 一律 FAIL；"后端没有转码能力"走的是下面 `Upd::Failure` 的错误路径。
                        let message = result.message.unwrap_or_default();
                        check("compat", false, &format!("prepare failed：{message}"));
                        return;
                    }
                }
            }
            Some(Upd::Prepared { .. }) => {}
            Some(Upd::Failure {
                code,
                detail_code,
                message,
            }) => {
                // 只有“后端转码不可用”才 SKIP，判据是协议错误码/detail_code（稳定契约），
                // 不看文案子串——文案随版本变，子串匹配会把真失败误当成环境缺能力。
                if conversion_unavailable(detail_code.as_deref(), &code) {
                    println!("SKIP compat —— 后端转码不可用（{code}）：{message}");
                } else {
                    check(
                        "compat",
                        false,
                        &format!("prepare 失败（{code}）：{message}"),
                    );
                }
                return;
            }
            Some(Upd::Error(message)) => {
                check("compat", false, &format!("prepare 报错：{message}"));
                return;
            }
            _ => {}
        }
    }
}

/// 后端“没有转码能力”的结构化判据（t31 F4）。
///
/// daemon 在转码被关闭 / 缺 ffmpeg 时回 `detail_code = CONVERSION_UNAVAILABLE`；
/// 旧 daemon 不带 detail 时才退回 `code` 判断。其余失败（素材损坏、ffmpeg 报错）
/// 都属于真失败，必须 FAIL。
fn conversion_unavailable(detail_code: Option<&str>, code: &str) -> bool {
    matches!(detail_code, Some("CONVERSION_UNAVAILABLE"))
        || (detail_code.is_none() && code == "CONVERSION_UNAVAILABLE")
}

/// 媒体源文件的指纹（大小 + 修改时间）：转码只该产出缓存副本，源文件必须原样。
fn source_stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()))
}

/// 取一条素材的兼容性判定（`get_media`）。
fn query_compat(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
    id: &str,
) -> Option<liuguang_ipc::protocol::CompatInfo> {
    let _ = tx.send(Cmd::MediaCompat { id: id.to_owned() });
    match next(rx, WAIT, |u| {
        matches!(u, Upd::Compat { .. } | Upd::Error(_))
    }) {
        Some(Upd::Compat { compat, .. }) => compat,
        _ => None,
    }
}

/// 为 apply 检查挑一个条目：图片 > `compat == playable` 的视频 > 先准备一条 NeedsConversion。
///
/// t18 F6：以前直接取 `rows.first()`，首条若是 8K H.264 / 10bit，apply 必然被
/// `NEEDS_PREPARATION` 拦下，于是"应用链路"没测到、自检却红了。
fn pick_apply_item(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
    media: Option<&Vec<MediaItem>>,
) -> Option<MediaItem> {
    let rows = media?;
    // 图片永远可直接应用（静态壁纸），优先用它验证"应用链路"本身。
    if let Some(image) = rows.iter().find(|item| item.kind == "image") {
        return Some(image.clone());
    }
    let mut fallback = None;
    for item in rows.iter().filter(|item| item.kind == "video") {
        match query_compat(tx, rx, &item.id).map(|compat| compat.state) {
            Some(CompatState::Playable) => return Some(item.clone()),
            Some(CompatState::NeedsConversion) if fallback.is_none() => {
                fallback = Some(item.clone());
            }
            _ => {}
        }
    }
    if let Some(item) = fallback {
        let _ = tx.send(Cmd::Prepare {
            path: item.path.clone(),
            force: false,
        });
        let deadline = Instant::now() + COMPAT_WAIT;
        // 与 `check_compat` 同样的道理（t31 F1）：冷缓存下第一次回包可能只是 Queued，
        // 单次等待会白等到超时；每 500ms 重发幂等查询，直到 Ready/Failed。
        let mut last_send = Instant::now();
        while Instant::now() < deadline {
            if last_send.elapsed() >= Duration::from_millis(500) {
                let _ = tx.send(Cmd::Prepare {
                    path: item.path.clone(),
                    force: false,
                });
                last_send = Instant::now();
            }
            match next(rx, Duration::from_millis(400), |u| {
                matches!(
                    u,
                    Upd::Prepared { .. } | Upd::Failure { .. } | Upd::Error(_)
                )
            }) {
                Some(Upd::Prepared { path, result }) if path == item.path => match result.state {
                    PrepareState::Ready => return Some(item),
                    PrepareState::Failed => break,
                    PrepareState::Queued | PrepareState::Running => {}
                },
                // 准备失败（含后端无转码能力）就不再等，退回第一项让 apply 自己暴露结论。
                Some(Upd::Failure { .. } | Upd::Error(_)) => break,
                _ => {}
            }
        }
    }
    rows.first().cloned()
}

/// 播放列表 CRUD：建一个临时列表、改名、确认列表里能看到、再删掉。
fn check_playlists(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
    media: Option<&Vec<MediaItem>>,
    check: &mut impl FnMut(&str, bool, &str),
) {
    let entries: Vec<String> = media
        .map(|rows| rows.iter().take(2).map(|row| row.id.clone()).collect())
        .unwrap_or_default();
    let name = format!("GUI 自检 {}", std::process::id());
    let _ = tx.send(Cmd::Playlists);
    let _ = next(rx, WAIT, |u| matches!(u, Upd::Playlists(_) | Upd::Error(_)));
    let _ = tx.send(Cmd::UpdatePlaylist(Box::new(PlaylistEdit {
        playlist_id: None,
        name: name.clone(),
        entries: entries.clone(),
        mode: "sequential".to_owned(),
        interval_seconds: 30,
        on_video_end: "next".to_owned(),
        output: None,
        no_repeat: false,
    })));
    let created: Option<Playlist> = match next(rx, WAIT, |u| {
        matches!(u, Upd::PlaylistSaved(_) | Upd::Error(_))
    }) {
        Some(Upd::PlaylistSaved(playlist)) => Some(*playlist),
        Some(Upd::Error(message)) => {
            check("playlist-create", false, &message);
            return;
        }
        _ => {
            check("playlist-create", false, "超时：没有收到保存回执");
            return;
        }
    };
    let Some(playlist) = created else {
        return;
    };
    check(
        "playlist-create",
        playlist.name == name && playlist.interval_seconds == 30,
        &format!("id={} name={}", playlist.id, playlist.name),
    );

    let renamed = format!("{name}（改名）");
    let _ = tx.send(Cmd::UpdatePlaylist(Box::new(PlaylistEdit {
        playlist_id: Some(playlist.id.clone()),
        name: renamed.clone(),
        entries,
        mode: "random".to_owned(),
        interval_seconds: 45,
        on_video_end: "loop".to_owned(),
        output: None,
        no_repeat: true,
    })));
    match next(rx, WAIT, |u| {
        matches!(u, Upd::PlaylistSaved(_) | Upd::Error(_))
    }) {
        Some(Upd::PlaylistSaved(saved)) => check(
            "playlist-update",
            saved.name == renamed && saved.mode == "random" && saved.interval_seconds == 45,
            &format!(
                "name={} mode={} interval={}s",
                saved.name, saved.mode, saved.interval_seconds
            ),
        ),
        Some(Upd::Error(message)) => check("playlist-update", false, &message),
        _ => check("playlist-update", false, "超时"),
    }

    let _ = tx.send(Cmd::Playlists);
    match next(rx, WAIT, |u| matches!(u, Upd::Playlists(_) | Upd::Error(_))) {
        Some(Upd::Playlists(list)) => check(
            "playlist-list",
            list.iter()
                .any(|entry| entry.id == playlist.id && entry.name == renamed),
            &format!("共 {} 个播放列表", list.len()),
        ),
        Some(Upd::Error(message)) => check("playlist-list", false, &message),
        _ => check("playlist-list", false, "超时"),
    }

    let _ = tx.send(Cmd::DeletePlaylist {
        playlist_id: playlist.id.clone(),
    });
    match next(rx, WAIT, |u| {
        matches!(u, Upd::PlaylistDeleted(_) | Upd::Error(_))
    }) {
        Some(Upd::PlaylistDeleted(id)) => check(
            "playlist-delete",
            id == playlist.id,
            &format!("已删除 {id}"),
        ),
        Some(Upd::Error(message)) => check("playlist-delete", false, &message),
        _ => check("playlist-delete", false, "超时"),
    }
}

/// 收藏/视图偏好往返：写进临时文件再读回来（不动用户真正的 gui.json）。
fn check_prefs(media: Option<&Vec<MediaItem>>, check: &mut impl FnMut(&str, bool, &str)) {
    let path = std::env::temp_dir().join(format!("liuguang-gui-prefs-{}.json", std::process::id()));
    let mut prefs = Prefs {
        view: "list".to_owned(),
        card_size: 300,
        sort: 3,
        filter: 7,
        show_details: false,
        ..Prefs::default()
    };
    if let Some(item) = media.and_then(|rows| rows.first()) {
        prefs.favorites.push(item.id.clone());
    }
    let saved = prefs.save(&path);
    let reloaded = Prefs::load(&path);
    let okay = saved.is_ok()
        && reloaded.view == "list"
        && reloaded.card_size == 300
        && reloaded.sort == 3
        && reloaded.filter == 7
        && !reloaded.show_details
        && reloaded.favorites == prefs.favorites;
    let _ = std::fs::remove_file(&path);
    check(
        "preferences",
        okay,
        &format!(
            "{} · view={} size={} sort={} filter={} 收藏 {} 项",
            path.display(),
            reloaded.view,
            reloaded.card_size,
            reloaded.sort,
            reloaded.filter,
            reloaded.favorites.len()
        ),
    );
}

/// 设置读写：加载真实配置 → 改几个字段 → 原子保存到临时文件 → 重读比对。
///
/// 不写回用户配置：这里只证明"设置页那套 Config 读写链路是通的"。
fn check_settings(config_path: &Path, check: &mut impl FnMut(&str, bool, &str)) {
    let loaded = Config::load(config_path);
    let path =
        std::env::temp_dir().join(format!("liuguang-gui-config-{}.toml", std::process::id()));
    let result = (|| -> Result<(usize, String, bool, usize), String> {
        let mut config = match &loaded {
            Ok(config) => config.clone(),
            Err(_) => Config::default(),
        };
        config.slideshow.interval_seconds = 601;
        config.performance.pause_on_fullscreen = false;
        config.media.video_dmabuf_rgb = "never".to_owned();
        config.validate().map_err(|error| error.to_string())?;
        let bytes = config
            .save_atomic(&path)
            .map_err(|error| error.to_string())?;
        let reread = Config::load(&path).map_err(|error| error.to_string())?;
        let okay = reread.slideshow.interval_seconds == 601
            && !reread.performance.pause_on_fullscreen
            && reread.media.video_dmabuf_rgb == "never";
        Ok((
            bytes,
            reread.media.video_dmabuf_rgb.clone(),
            okay,
            reread.slideshow.interval_seconds as usize,
        ))
    })();
    let _ = std::fs::remove_file(&path);
    match result {
        Ok((bytes, rgb, okay, interval)) => check(
            "settings",
            okay,
            &format!(
                "{} 字节 · interval={interval}s · rgb={rgb} · 源配置{}",
                bytes,
                if loaded.is_ok() {
                    "已加载"
                } else {
                    "缺失（用默认值）"
                }
            ),
        ),
        Err(message) => check("settings", false, &message),
    }
}

/// 从配置文件里取该输出当前的 fit/zoom/offset/rotation。
///
/// 读不到就用 `cover/1.0/0,0/0°` 兜底：自检宁可退回默认，也不能因为读配置失败而不测 apply。
fn restore_settings(config_path: &Path, output: &str) -> RenderSetting {
    let Ok(config) = Config::load(config_path) else {
        return RenderSetting::default();
    };
    let Some(entry) = config.outputs.get(output) else {
        return RenderSetting::default();
    };
    RenderSetting {
        fit: entry.fit.as_str().to_owned(),
        zoom: entry.zoom,
        offset_x: entry.offset_x,
        offset_y: entry.offset_y,
        rotation: details::rotation_degrees(entry.rotation),
    }
}

/// 把自检改过的东西放回去：壁纸、`pinned`、暂停状态。
///
/// 顺序有讲究：先 `apply`（它会顺手把 `pinned` 置 true，那是 `set` 的语义），
/// 再按原值改 `pinned`，最后才恢复暂停——否则暂停会被后面的 `apply` 冲掉。
fn restore(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
    snapshot: &Snapshot,
) {
    let Some(media) = snapshot.media.as_deref() else {
        println!("（自检开始时没有壁纸，无需恢复）");
        return;
    };
    let setting = &snapshot.setting;
    let _ = tx.send(Cmd::Apply(Box::new(ipc::ApplyRequest {
        path: media.to_owned(),
        output: snapshot.output.clone(),
        fit: setting.fit.clone(),
        zoom: setting.zoom,
        offset_x: setting.offset_x,
        offset_y: setting.offset_y,
        rotation: setting.rotation,
        brightness: 1.0,
        pin: snapshot.pinned,
    })));
    let restored = matches!(
        next(rx, WAIT, |u| matches!(u, Upd::Applied(_) | Upd::Error(_))),
        Some(Upd::Applied(_))
    );
    if !restored {
        println!("（恢复原壁纸失败：{media}）");
        return;
    }
    if !snapshot.pinned {
        // `apply` 会把它置成 true，这里按原值改回来。
        let _ = tx.send(Cmd::SetPinned {
            pinned: false,
            output: snapshot.output.clone(),
        });
        let _ = next(rx, WAIT, |u| matches!(u, Upd::Pinned(_) | Upd::Error(_)));
    }
    if snapshot.paused {
        let _ = tx.send(Cmd::Pause);
        let _ = next(rx, WAIT, |u| matches!(u, Upd::Paused(_) | Upd::Error(_)));
    }
    println!("（已恢复原壁纸：{}）", short(media));
}

/// 拉媒体列表：`list_media` 首次调用会**顺带启动后台扫描**并返回空页 +
/// `scanning=true`，所以这里必须像窗口一样自己重试，而不是看到空就判失败。
fn wait_media(
    tx: &std::sync::mpsc::Sender<Cmd>,
    rx: &async_channel::Receiver<Upd>,
) -> Result<Vec<MediaItem>, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let _ = tx.send(Cmd::Media);
        match next(
            rx,
            deadline.saturating_duration_since(Instant::now()),
            |u| matches!(u, Upd::Media { .. } | Upd::Error(_)),
        ) {
            Some(Upd::Media {
                items,
                total,
                scanning,
            }) => {
                if !items.is_empty() {
                    return Ok(items);
                }
                if !scanning {
                    return Err(format!(
                        "媒体库为空（total={total}）：检查配置里的 [library] 段"
                    ));
                }
                if Instant::now() >= deadline {
                    return Err("超时：媒体扫描一直没结束".to_owned());
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            Some(Upd::Error(message)) => return Err(message),
            _ => return Err("超时：没有收到媒体列表".to_owned()),
        }
    }
}

/// 等一条满足 `pred` 的更新（其余状态更新直接跳过）。
///
/// t31 F1：旧实现用 `recv_blocking()`，没有更新时会**永远阻塞**——调用方给的
/// `timeout`（以及外层的 `COMPAT_WAIT`）根本不会生效，冷缓存下自检直接挂死。
/// 现在用 `try_recv` + 20ms 小睡轮询：到点返回 `None`，让调用方按 deadline 判失败。
/// 20ms 是轮询间隔的上限误差：policy 是「100ms 级」的超时，不需要更精细。
fn next(
    rx: &async_channel::Receiver<Upd>,
    timeout: Duration,
    pred: impl Fn(&Upd) -> bool,
) -> Option<Upd> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(update) => {
                if pred(&update) {
                    return Some(update);
                }
            }
            Err(async_channel::TryRecvError::Empty) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                std::thread::sleep(left.min(Duration::from_millis(20)));
            }
            Err(async_channel::TryRecvError::Closed) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// 前三个文件名，用于在日志里说明"确实拿到了具体条目"。
fn sample(rows: &[MediaItem]) -> String {
    let names: Vec<&str> = rows.iter().take(3).map(|row| row.name.as_str()).collect();
    let suffix = if rows.len() > 3 { " …" } else { "" };
    format!("{}{suffix}", names.join(", "))
}

/// 路径太长时只留文件名，打印好看一点。
fn short(path: &str) -> String {
    PathBuf::from(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_returns_none_when_timeout_expires_without_updates() {
        let (_tx, rx) = async_channel::unbounded::<Upd>();
        let started = Instant::now();
        let got = next(&rx, Duration::from_millis(60), |_| true);
        assert!(
            got.is_none(),
            "没有更新时 next 必须返回 None，而不是永久阻塞"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "60ms 的超时不该等这么久：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn next_skips_unrelated_updates_and_returns_on_match() {
        let (tx, rx) = async_channel::unbounded::<Upd>();
        tx.send_blocking(Upd::Notice("无关".to_owned())).unwrap();
        let got = next(&rx, Duration::from_millis(60), |u| {
            matches!(u, Upd::Error(_))
        });
        assert!(got.is_none(), "不匹配的更新必须被跳过，且到点返回 None");

        tx.send_blocking(Upd::Error("命中的错误".to_owned()))
            .unwrap();
        tx.send_blocking(Upd::Notice("之后的更新".to_owned()))
            .unwrap();
        let got = next(&rx, Duration::from_millis(200), |u| {
            matches!(u, Upd::Error(_))
        });
        assert!(matches!(got, Some(Upd::Error(message)) if message == "命中的错误"));
        // 命中即返回：后面的 Notice 仍留在队列里，没有被这次调用吞掉。
        assert!(matches!(rx.try_recv(), Ok(Upd::Notice(_))));
    }

    #[test]
    fn conversion_unavailable_uses_structured_codes() {
        assert!(conversion_unavailable(
            Some("CONVERSION_UNAVAILABLE"),
            "UNSUPPORTED_CODEC"
        ));
        assert!(conversion_unavailable(None, "CONVERSION_UNAVAILABLE"));
        // 有 detail 但不是“转码不可用” ⇒ 真失败，不能 SKIP。
        assert!(!conversion_unavailable(
            Some("COMPAT_UNSUPPORTED"),
            "UNSUPPORTED_CODEC"
        ));
        assert!(!conversion_unavailable(None, "UNSUPPORTED_CODEC"));
    }
}
