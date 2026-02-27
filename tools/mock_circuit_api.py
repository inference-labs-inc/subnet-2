import http.server
import json
import os
import sys
import threading

CACHE_DIR = os.path.expanduser("~/.bittensor/subnet-2/circuit_cache")

all_circuits = {}

for entry in (os.listdir(CACHE_DIR) if os.path.exists(CACHE_DIR) else []):
    if not entry.startswith("model_") or len(entry) != 70:
        continue
    circuit_id = entry[6:]
    metadata_path = os.path.join(CACHE_DIR, entry, "circuit_metadata.json")
    if not os.path.exists(metadata_path):
        continue
    with open(metadata_path) as f:
        metadata = json.load(f)
    all_circuits[circuit_id] = metadata

active_ids = set()
lock = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        sys.stderr.write(f"[mock-api] {format % args}\n")

    def do_GET(self):
        if self.path == "/circuits":
            with lock:
                circuits = []
                for cid in active_ids:
                    if cid in all_circuits:
                        circuits.append({
                            "id": cid,
                            "metadata": all_circuits[cid],
                            "files": {},
                        })
            body = json.dumps({"circuits": circuits}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == "/admin/status":
            with lock:
                status = {
                    "available": {cid: all_circuits[cid]["name"] for cid in all_circuits},
                    "active": {cid: all_circuits[cid]["name"] for cid in active_ids if cid in all_circuits},
                }
            body = json.dumps(status, indent=2).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path.startswith("/admin/activate/"):
            cid = self.path.split("/")[-1]
            with lock:
                if cid in all_circuits:
                    active_ids.add(cid)
                    name = all_circuits[cid]["name"]
                    self.send_response(200)
                    body = json.dumps({"activated": cid, "name": name}).encode()
                else:
                    self.send_response(404)
                    body = json.dumps({"error": f"unknown circuit {cid}"}).encode()
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        elif self.path.startswith("/admin/deactivate/"):
            cid = self.path.split("/")[-1]
            with lock:
                active_ids.discard(cid)
            self.send_response(200)
            body = json.dumps({"deactivated": cid}).encode()
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        elif self.path == "/admin/activate-all":
            with lock:
                active_ids.update(all_circuits.keys())
            self.send_response(200)
            body = json.dumps({"activated": list(active_ids)}).encode()
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        elif self.path == "/admin/deactivate-all":
            with lock:
                active_ids.clear()
            self.send_response(200)
            body = json.dumps({"deactivated": "all"}).encode()
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        else:
            self.send_error(404)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8888
    print(f"Mock Circuit API on port {port}")
    print("Available circuits:")
    for cid, meta in all_circuits.items():
        print(f"  {cid[:12]}... {meta['name']}")
    print("\nEndpoints:")
    print("  GET  /circuits              - active circuit list")
    print("  GET  /admin/status          - all available + active")
    print("  POST /admin/activate/{id}   - activate a circuit")
    print("  POST /admin/deactivate/{id} - deactivate a circuit")
    print("  POST /admin/activate-all    - activate all")
    print("  POST /admin/deactivate-all  - deactivate all")
    server = http.server.HTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()
