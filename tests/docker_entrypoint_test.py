"""Smoke-test Docker's startup environment and fail-closed public-bind gate."""

import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def main():
    with tempfile.TemporaryDirectory(prefix="megu-entrypoint-") as temporary:
        folder = Path(temporary)
        data = folder / "data"
        data.mkdir()
        stub = folder / "server-stub.sh"
        stub.write_text(
            "#!/bin/sh\n"
            'printf "SERVER_EXEC online=%s bind=%s args=%s\\n" '
            '"$MEGU_ONLINE_MODE" "$MEGU_BIND_ADDR" "$*"\n',
            encoding="utf-8",
        )
        stub.chmod(0o700)
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "EULA": "TRUE",
            "MEGU_DATA_DIR": str(data),
            "MEGU_BIN": str(stub),
        }

        def entrypoint(environment):
            return subprocess.run(
                ["bash", str(ROOT / "docker" / "entrypoint.sh"), "--smoke"],
                env=environment,
                cwd=folder,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )

        default = entrypoint(env)
        assert default.returncode == 0, (default.returncode, default.stderr)
        assert "SERVER_EXEC online=1 bind=0.0.0.0 args=--smoke" in default.stdout
        assert (data / "eula.txt").read_text(encoding="utf-8").endswith("eula=true\n")

        # An explicit offline setting still fails closed before the binary
        # starts, even though the image allows an externally reachable bind.
        offline = entrypoint({**env, "MEGU_ONLINE_MODE": "0"})
        assert offline.returncode == 2, (offline.returncode, offline.stderr)
        assert "refusing public bind with unverified offline identities" in offline.stderr
        assert "SERVER_EXEC" not in offline.stdout

    print("docker entrypoint smoke: PASS (verified default and offline-public rejection)")


if __name__ == "__main__":
    main()
