import argparse
import base64
import json
import pathlib
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import jwt
from cryptography.hazmat.primitives.asymmetric import rsa

KID = "reversalgraph-ci-key"
ISSUER = "http://127.0.0.1:8765"
AUDIENCE = "https://localhost:8443/mcp"


def b64url_uint(value: int) -> str:
    size = max(1, (value.bit_length() + 7) // 8)
    return base64.urlsafe_b64encode(value.to_bytes(size, "big")).rstrip(b"=").decode()


def make_fixture(out: pathlib.Path):
    out.mkdir(parents=True, exist_ok=True)
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    numbers = key.public_key().public_numbers()
    jwks = {
        "keys": [
            {
                "kty": "RSA",
                "kid": KID,
                "use": "sig",
                "alg": "RS256",
                "n": b64url_uint(numbers.n),
                "e": b64url_uint(numbers.e),
            }
        ]
    }
    (out / "jwks.json").write_text(json.dumps(jwks), encoding="utf-8")

    now = int(time.time())

    def token(scope: str, *, audience=AUDIENCE, expires=now + 3600):
        return jwt.encode(
            {
                "sub": "chatgpt-ci-user",
                "iss": ISSUER,
                "aud": audience,
                "iat": now,
                "nbf": now - 1,
                "exp": expires,
                "scope": scope,
            },
            key,
            algorithm="RS256",
            headers={"kid": KID},
        )

    (out / "read.token").write_text(token("graph:read"), encoding="utf-8")
    (out / "write.token").write_text(
        token("graph:read graph:write"), encoding="utf-8"
    )
    (out / "wrong-audience.token").write_text(
        token("graph:read graph:write", audience="https://wrong.example/mcp"),
        encoding="utf-8",
    )
    (out / "expired.token").write_text(
        token("graph:read graph:write", expires=now - 120),
        encoding="utf-8",
    )


class Handler(BaseHTTPRequestHandler):
    fixture_dir: pathlib.Path

    def do_GET(self):
        if self.path == "/jwks.json":
            data = (self.fixture_dir / "jwks.json").read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        if self.path in (
            "/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration",
        ):
            body = json.dumps(
                {
                    "issuer": ISSUER,
                    "authorization_endpoint": f"{ISSUER}/authorize",
                    "token_endpoint": f"{ISSUER}/token",
                    "jwks_uri": f"{ISSUER}/jwks.json",
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "scopes_supported": ["graph:read", "graph:write"],
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        self.send_response(404)
        self.end_headers()

    def log_message(self, fmt, *args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()

    out = pathlib.Path(args.output).resolve()
    make_fixture(out)
    Handler.fixture_dir = out

    print(f"OAUTH_FIXTURE_READY {ISSUER}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
