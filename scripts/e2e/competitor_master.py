#!/usr/bin/env python3
"""占住 DRM master 的竞争者：用于验收 daemon 的自愈（无显示器也可测）。

用法：LIUGUANG_CARD=/dev/dri/cardN python3 scripts/e2e/competitor_master.py &
释放：kill %1（SIGTERM 处理函数会先 DROP_MASTER 再退出）
"""
import fcntl, os, signal, sys, time

CARD = os.environ.get("LIUGUANG_CARD", "/dev/dri/card1")
SET_MASTER = 0x641E
DROP_MASTER = 0x641F
fd = os.open(CARD, os.O_RDWR)
try:
    fcntl.ioctl(fd, SET_MASTER)
    print("competitor: master acquired", flush=True)
except OSError as e:
    print(f"competitor: SET_MASTER failed: {e}", flush=True)
    os.close(fd)
    sys.exit(1)

def bye(*_):
    try:
        fcntl.ioctl(fd, DROP_MASTER)
        print("competitor: master dropped", flush=True)
    except OSError as e:
        print(f"competitor: DROP_MASTER failed: {e}", flush=True)
    os.close(fd)
    sys.exit(0)

signal.signal(signal.SIGTERM, bye)
signal.signal(signal.SIGINT, bye)
print("competitor: holding master", flush=True)
while True:
    time.sleep(1)
