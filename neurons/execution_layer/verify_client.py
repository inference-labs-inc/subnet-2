from __future__ import annotations

import logging
import os
import socket
import struct
import subprocess
import threading
import time
from pathlib import Path
from typing import Any

import msgpack

SOCKET_PATH = "/tmp/sn2-verify.sock"
SHM_DIR = "/dev/shm" if os.path.isdir("/dev/shm") else "/tmp"


def _recvall(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("socket closed during recv")
        buf.extend(chunk)
    return bytes(buf)


class VerifyClient:
    def __init__(self, socket_path: str = SOCKET_PATH):
        self._socket_path = socket_path
        self._local = threading.local()
        self._process: subprocess.Popen | None = None
        self._binary_path: str | None = None

    def _find_binary(self) -> str | None:
        candidates = [
            Path(__file__).parent.parent.parent
            / "sn2-verify"
            / "target"
            / "release"
            / "sn2-verify",
        ]
        for p in candidates:
            if p.exists():
                return str(p)
        return None

    @staticmethod
    def _probe_socket(path: str) -> bool:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.settimeout(1.0)
            s.connect(path)
            return True
        except (ConnectionRefusedError, OSError):
            return False
        finally:
            s.close()

    def start_service(self) -> None:
        if Path(self._socket_path).exists():
            if self._probe_socket(self._socket_path):
                logging.info("sn2-verify already running at %s", self._socket_path)
                return
            os.unlink(self._socket_path)

        binary = self._find_binary()
        if binary is None:
            raise FileNotFoundError("sn2-verify binary not found")

        self._binary_path = binary
        env = os.environ.copy()
        env["SN2_VERIFY_SOCK"] = self._socket_path
        self._process = subprocess.Popen(
            [binary],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        for _ in range(50):
            if Path(self._socket_path).exists() and self._probe_socket(
                self._socket_path
            ):
                logging.info("sn2-verify started, pid=%d", self._process.pid)
                return
            time.sleep(0.1)

        raise TimeoutError("sn2-verify did not start within 5s")

    def _get_connection(self) -> socket.socket:
        sock = getattr(self._local, "sock", None)
        if sock is not None:
            return sock
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self._socket_path)
        self._local.sock = sock
        return sock

    def _close_connection(self) -> None:
        sock = getattr(self._local, "sock", None)
        if sock is not None:
            try:
                sock.close()
            except OSError:
                pass
            self._local.sock = None

    def verify_sync(
        self,
        request_id: str,
        circuit_path: str,
        witness_hex: str,
        proof_hex: str,
        num_inputs: int,
        expected_inputs: list[float] | None = None,
        pcs_type: str = "Hyrax",
    ) -> dict[str, Any]:
        shm_path = os.path.join(SHM_DIR, f"sn2_witness_{request_id}")
        with open(shm_path, "w") as f:
            f.write(witness_hex)

        msg = msgpack.packb(
            {
                "request_id": request_id,
                "circuit_path": circuit_path,
                "witness_shm_path": shm_path,
                "proof_hex": proof_hex,
                "num_inputs": num_inputs,
                "expected_inputs": expected_inputs,
                "pcs_type": pcs_type,
            }
        )

        frame = struct.pack(">I", len(msg)) + msg

        try:
            for attempt in range(2):
                try:
                    sock = self._get_connection()
                    sock.sendall(frame)
                    length_bytes = _recvall(sock, 4)
                    length = struct.unpack(">I", length_bytes)[0]
                    data = _recvall(sock, length)
                    return msgpack.unpackb(data, raw=False)
                except (ConnectionError, BrokenPipeError, OSError):
                    self._close_connection()
                    if attempt == 1:
                        raise
        except Exception:
            try:
                os.unlink(shm_path)
            except OSError:
                pass
            raise

        raise ConnectionError("verify_sync failed after retries")

    def shutdown(self) -> None:
        self._close_connection()
        if self._process is not None:
            self._process.terminate()
            try:
                self._process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait()
            self._process = None
