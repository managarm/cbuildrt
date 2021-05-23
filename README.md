# cbuildrt: Containerized build runtime

`cbuildrt` implements a minimal unprivileged container runtime for use in
[xbstrap](https://github.com/managarm/xbstrap).
It tries to isolate the containerized process from the host environment
in order to achieve reproducible builds.

Note that in contrast to runtimes such as [`runc`](https://github.com/opencontainers/runc),
`cbuildrt` does not try to protect against malicious sandbox escapes.
