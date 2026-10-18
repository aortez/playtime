# Reviewed synthetic capture v1

`reference.tar` is the exact 10,240-byte reference specified in issue #54:
SHA-256 `e465b5958f9892de45a47cfe25802ce2330d1753fc8eb116a4e77dd1ae3b092e`.
It contains six logical members and seven physical headers (one PAX path override):

| Path | Type/mode | Bytes or target |
|---|---|---|
| a.txt | regular 0640 | `alpha` + LF |
| alias | hardlink 0640 | `a.txt` |
| dir/ | directory 0750 | no payload |
| dir/é.txt | regular 0644 | `beta` + LF |
| outside | symlink 0777 | `../../outside` |
| run.sh | regular 0755 | `#!/bin/sh` + LF + `exit 0` + LF |

uid/gid are 1000, uname/gname empty, mtime zero. This is synthetic data; no script
in the tar is executed. The bytes were reproduced with host Python `tarfile`,
compared to the issue's already-reviewed digest, and then independently matched by
the embedded exporter in the locked Docker image. The Rust parser checksums and
hashes the same immutable bytes. Python tests independently scan physical headers
and inspect them with `tarfile` without extracting. There is no golden regeneration
command in the tests. Contract changes require review, not automatic replacement.
