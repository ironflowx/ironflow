"""Call a fresh engine over a loopback TLS proxy with the extracted release binary."""

import http.client
import http.server
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import threading
import time
import urllib.request


class Proxy(http.server.BaseHTTPRequestHandler):
    def forward(self):
        upstream = http.client.HTTPConnection(self.server.engine, timeout=5)
        try:
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            upstream.request(self.command, self.path, body, dict(self.headers.items()))
            response = upstream.getresponse()
            data = response.read()
            self.send_response(response.status)
            self.send_header("Content-Type", response.headers["Content-Type"])
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        finally:
            upstream.close()

    do_GET = forward
    do_POST = forward


def ready_url(process, log):
    for _ in range(200):
        for line in log.read_text().splitlines():
            # The writer may be between writing a log record and writing its newline.
            try:
                fields = json.loads(line)["fields"]
            except json.JSONDecodeError:
                continue
            if fields["message"] == "listening":
                url = fields["url"]
                try:
                    with urllib.request.urlopen(url + "/ready", timeout=1) as response:
                        if response.status == 200:
                            return url
                except OSError:
                    pass
        if process.poll() is not None:
            break
        time.sleep(0.1)
    raise RuntimeError(f"archive's host did not become ready\n{log.read_text()}")


def main():
    binary, work = map(Path, sys.argv[1:])
    data = work / "engine"
    log = work / "serve.log"
    fixture = Path(__file__).resolve().parents[1] / "crates/ironflowx-bin/tests/tls"
    cert = fixture / "cert.pem"
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, fixture / "key.pem")
    env = {name: value for name, value in os.environ.items()
           if not name.startswith("IRONFLOWX_") and name != "SSL_CERT_DIR"}
    with log.open("w") as stderr, subprocess.Popen(
        [binary, "serve", "--listen", "127.0.0.1:0", "--data-dir", data], env=env, stderr=stderr,
    ) as host:
        try:
            engine = ready_url(host, log).removeprefix("http://")
            with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy) as server:
                server.engine = engine
                server.socket = tls.wrap_socket(server.socket, server_side=True)
                thread = threading.Thread(target=server.serve_forever)
                thread.start()
                try:
                    answer = subprocess.check_output(
                        [binary, "whoami", "--url", f"https://localhost:{server.server_port}",
                         "--key-file", data / "admin.key"],
                        env={**env, "SSL_CERT_FILE": str(cert)}, timeout=10,
                    )
                    if sorted(json.loads(answer)["scopes"]) != ["admin", "execute", "read", "write"]:
                        sys.exit("archive's HTTPS whoami did not authenticate as admin")
                    print("archive's HTTPS whoami authenticated as admin")
                finally:
                    server.shutdown()
                    thread.join()
        finally:
            host.terminate()
            try:
                host.wait(timeout=12)
            except subprocess.TimeoutExpired:
                host.kill()
                raise


if __name__ == "__main__":
    main()
