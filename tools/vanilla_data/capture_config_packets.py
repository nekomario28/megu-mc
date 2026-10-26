#!/usr/bin/env python3
"""Record the Configuration-phase packets a vanilla 1.21.1 server sends, as config_packets.bin.

  capture_config_packets.py <server.jar> <out.bin> --accept-eula

Starts the given server jar in a scratch directory (offline mode, loopback only, a free port,
no compression), connects as a bare protocol-767 client, answers its Select Known Packs with
"no known packs" so the server sends full registry data, and writes every packet the server
sends until Finish Configuration, each as `varint length | varint packet id | payload`.
That is the format `load_config_packets()` in src/main.rs replays.

Running the server is subject to Mojang's EULA (https://aka.ms/MinecraftEULA); the script writes
eula.txt in its scratch directory only when you pass --accept-eula, which is you saying you
accept it. Needs Java 21. Takes about 20 s (the server generates its spawn area).
"""
import os, shutil, socket, struct, subprocess, sys, tempfile, time

PROTOCOL = 767


def varint(n):
    out = bytearray()
    n &= 0xFFFFFFFF
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def mc_str(s):
    b = s.encode()
    return varint(len(b)) + b


def read_varint(sock):
    n = shift = 0
    while True:
        c = sock.recv(1)
        if not c:
            raise EOFError("server closed the connection")
        n |= (c[0] & 0x7F) << shift
        shift += 7
        if not c[0] & 0x80:
            return n


def read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("server closed the connection")
        buf += chunk
    return buf


def read_packet(sock):
    body = read_exact(sock, read_varint(sock))
    pid, i = 0, 0
    shift = 0
    while True:
        b = body[i]
        i += 1
        pid |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            break
    return pid, body


def send(sock, pid, payload=b""):
    body = varint(pid) + payload
    sock.sendall(varint(len(body)) + body)


def main():
    if len(sys.argv) != 4 or sys.argv[3] != "--accept-eula":
        print(__doc__, file=sys.stderr)
        return 2
    jar, out = sys.argv[1], sys.argv[2]
    work = tempfile.mkdtemp(prefix="mc-spine-config-capture-")
    proc = None
    try:
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        shutil.copyfile(jar, os.path.join(work, "server.jar"))
        with open(os.path.join(work, "eula.txt"), "w") as f:
            f.write("eula=true\n")
        with open(os.path.join(work, "server.properties"), "w") as f:
            f.write(
                "online-mode=false\nserver-ip=127.0.0.1\nserver-port=%d\n"
                "network-compression-threshold=-1\nenable-status=false\nview-distance=2\n"
                "simulation-distance=2\nspawn-protection=0\nlevel-seed=0\n"
                "enforce-secure-profile=false\n" % port
            )
        log = open(os.path.join(work, "server.log"), "wb")
        proc = subprocess.Popen(
            ["java", "-jar", "server.jar", "nogui"], cwd=work, stdout=log, stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
        )
        sock = None
        deadline = time.time() + 180
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=5)
                break
            except OSError:
                time.sleep(0.5)
        if sock is None:
            log.flush()
            print(open(os.path.join(work, "server.log"), errors="replace").read()[-2000:], file=sys.stderr)
            print("server did not start listening", file=sys.stderr)
            return 1
        sock.settimeout(60)
        send(sock, 0, varint(PROTOCOL) + mc_str("127.0.0.1") + struct.pack(">H", port) + varint(2))
        send(sock, 0, mc_str("Capture") + bytes(16))  # Login Start: name, uuid
        pid, body = read_packet(sock)
        if pid != 2:
            print("expected Login Success (2), got packet %d" % pid, file=sys.stderr)
            return 1
        send(sock, 3)  # Login Acknowledged
        packets = []
        while True:
            pid, body = read_packet(sock)
            packets.append(body)
            if pid == 0x0E:  # Select Known Packs: answer "none"
                send(sock, 0x07, varint(0))
            if pid == 0x03:  # Finish Configuration
                break
        with open(out, "wb") as f:
            for b in packets:
                f.write(varint(len(b)) + b)
        print("wrote %s: %d packets, %d bytes" % (out, len(packets), os.path.getsize(out)))
        return 0
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
