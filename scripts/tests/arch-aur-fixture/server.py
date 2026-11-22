from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
import json
import sys


class FixtureHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        url = urlsplit(self.path)
        if url.path not in ("/rpc", "/rpc/", "/rpc/v5/info", "/rpc/v5/search"):
            return super().do_GET()
        query = parse_qs(url.query)
        self.rpc_response(url.path, query)

    def do_POST(self):
        url = urlsplit(self.path)
        if url.path not in ("/rpc", "/rpc/", "/rpc/v5/info"):
            return self.send_error(404)
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        self.rpc_response(url.path, parse_qs(body))

    def rpc_response(self, path, query):
        package_names = query.get("arg[]", query.get("arg", []))
        query_type = query.get("type", ["search" if path.endswith("/search") else "info"])[0]
        results = []
        for package_name in package_names:
            if (
                not package_name.startswith("dek-aur-")
                or any(char not in "abcdefghijklmnopqrstuvwxyz0123456789-" for char in package_name)
            ):
                continue
            metadata = Path(self.directory, package_name + ".json")
            if metadata.is_file():
                results.append(json.loads(metadata.read_text()))
        response = json.dumps(
            {
                "version": 5,
                "type": "search" if query_type == "search" else "multiinfo",
                "resultcount": len(results),
                "results": results,
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


server = ThreadingHTTPServer(("127.0.0.1", 18765), partial(FixtureHandler, directory=sys.argv[1]))
server.serve_forever()
