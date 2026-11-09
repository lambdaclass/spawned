"""Bounded loopback-only Engine API mock; no real Ethereum traffic or secrets."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import threading
import time

p = argparse.ArgumentParser()
p.add_argument('--scenario', choices=['immediate', 'delay20ms', 'retry503'], required=True)
p.add_argument('--ready', type=Path, required=True)
p.add_argument('--result', type=Path, required=True)
args = p.parse_args()
lock = threading.Lock()
attempts = {}
counts = {'requests': 0, 'injected_failures': 0, 'connections': 0}


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.connection.settimeout(8)
        with lock:
            counts['connections'] += 1

    def log_message(self, *_):
        pass

    def do_POST(self):
        size = int(self.headers.get('Content-Length', '0'))
        assert 0 < size <= 16384
        call = json.loads(self.rfile.read(size))
        assert call['method'] == 'engine_forkchoiceUpdatedV3'
        key = call['params'][0]['headBlockHash']
        with lock:
            assert len(attempts) < 100_100
            attempts[key] = attempts.get(key, 0) + 1
            attempt = attempts[key]
            counts['requests'] += 1
        code = 200
        if args.scenario == 'retry503' and attempt == 1:
            code = 503
            body = b'injected temporary failure'
            with lock:
                counts['injected_failures'] += 1
        else:
            if args.scenario == 'delay20ms':
                time.sleep(.020)
            body = json.dumps({'jsonrpc': '2.0', 'id': call['id'], 'result': {
                'payloadStatus': {'status': 'SYNCING', 'latestValidHash': None, 'validationError': None},
                'payloadId': None}}).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_GET(self):
        assert self.path == '/finish'
        with lock:
            result = {**counts, 'unique_calls': len(attempts), 'scenario': args.scenario}
        args.result.write_text(json.dumps(result, indent=2) + '\n')
        self.send_response(200)
        self.send_header('Content-Length', '2')
        self.end_headers()
        self.wfile.write(b'ok')
        threading.Thread(target=self.server.shutdown, daemon=True).start()


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
args.ready.write_text(json.dumps({'endpoint': f'http://127.0.0.1:{server.server_port}'}))
server.serve_forever(poll_interval=.05)
server.server_close()
