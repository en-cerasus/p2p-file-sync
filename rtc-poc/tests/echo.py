import os
import socket
import sys

ADDRESS = ("127.0.0.1", 9765 if sys.argv[1] == "server" else 9865)

if sys.argv[1] == "server":
    with socket.socket() as listener:
        listener.bind(ADDRESS)
        listener.listen(1)
        with listener.accept()[0] as connection:
            while data := connection.recv(16384):
                connection.sendall(data)
elif sys.argv[1] == "client":
    data = os.urandom(100_000)
    with socket.create_connection(ADDRESS, timeout=15) as connection:
        connection.settimeout(15)
        connection.sendall(data)
        received = bytearray()
        while len(received) < len(data) and (chunk := connection.recv(16384)):
            received.extend(chunk)
    assert received == data, f"echo mismatch: {len(received)} of {len(data)} bytes"
    print(f"WebRTC TCP tunnel echoed {len(data)} bytes")
else:
    raise SystemExit("expected server or client")
