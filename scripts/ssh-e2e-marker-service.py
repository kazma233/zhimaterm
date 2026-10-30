#!/usr/bin/env python3
# SSH e2e 补强验证的判别服务:监听一个「仅目标机可达」的地址,每连接回一行
# 标记后保持到对端关闭。用法(mirrored 网络的 WSL 内,IP 先 ip addr add):
#
#   ip addr add 10.99.99.1/32 dev lo
#   python3 scripts/ssh-e2e-marker-service.py
#
# 客户端宿主对该地址无路由,经跳板+本地转发能取回标记串即严格证明
# direct-tcpip 由目标机发起。方法与断言样例见
# docs/context/ssh-e2e-wsl-tester-verification.md。
import socket
import threading
import time

LISTEN_ADDR = ("10.99.99.1", 2468)
MARKER = b"SECRET-FROM-TESTER-10-99-99-1\n"


def handle(conn):
    try:
        conn.sendall(MARKER)
        while conn.recv(4096):
            pass
    except OSError:
        pass
    finally:
        conn.close()


def serve():
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(LISTEN_ADDR)
    server.listen(8)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


threading.Thread(target=serve, daemon=True).start()
while True:
    time.sleep(60)
