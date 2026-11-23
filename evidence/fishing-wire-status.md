# Fishing wire check status

- Target: `target/release/megu` (protocol 767)
- Python compile: PASS — `python3 -m py_compile tests/fishing_wire.py tests/fishing_wire_test.py`
- Pure helper tests: PASS — `python3 tests/fishing_wire_test.py -v` (4 tests)
- Live attempt: NOT RUN to gameplay. `python3 tests/fishing_wire.py target/release/megu` returned exit 1 with JSON `{"failure": "PermissionError: [Errno 1] Operation not permitted", "ok": false, "protocol": 767, "skipped": {}, "stages": {}}` before the local server started or a client logged in.
- The binary is present and has executable mode (`-rwxr-xr-x`, 14,525,256 bytes). The execution restriction prevented runtime verification; bobber spawn, bite, and catch remain unverified.
- Bite-signal boundary in source: the server flips the bobber's internal `bit` after its wait but this tick path sends only periodic entity teleports; it does not emit a bite sound/particle or bite-specific submerged movement. If execution is available, the probe records this signal as skipped and still checks for a fish inventory update after reeling.

## Resolution (2026-11-24, local live run)

`{"ok": true}` on the release binary — cast produces bobber spawn
(entity type 129), reeling produces the fish item inventory update
(item 935), and the two unimplemented pieces stay honestly `skipped`:
no bobber spawn follows the air-use packet, and the server flips the
internal bobber bit without emitting a bite sound, particle, or
submerge movement.
