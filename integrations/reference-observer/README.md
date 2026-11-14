# Fixed-reference observer

Host-side companion for reference-backed contract discovery. It is not a verifier or
an implementation oracle. A trusted host configures a digest-pinned container image
and one absolute executable path. Model requests contain only `argv`, optional
UTF-8 `stdin`, and relative UTF-8 fixture files.

Each observation runs in a fresh, unprivileged, network-disconnected, read-only
container. No host bind mounts, credentials, model transport, benchmark tests or
solutions are supplied. Only a temporary scratch filesystem is writable. The
reference sees a fresh HOME and explicit environment, which are recorded alongside
inputs, exact stdout/stderr bytes (base64), decoded display strings, exit status and
reference identity. A host evidence record is persisted before it is returned.
The caller must already be authorized to execute the configured reference. The host must verify that the pinned image itself contains no hidden tests, solutions, credentials or unrelated private data: absence of extra mounts alone does not establish that.

The host records incomplete output and infrastructure failures distinctly.
Operational probe limits (30 seconds, 256 KiB per output stream, container resource
limits) are not language-validation limits. A timeout, inaccessible dependency,
truncated stream, or unavailable daemon does not establish impossible requirements
or justify weakening a specification. Repeated observations do not prove equivalence.

`Observer` is callable and returns an object envelope with `id`, `provenance`,
`content`, `content_hash` and `complete`. A host can expose it through a narrowly
configured local observation endpoint; never accept image, executable, shell,
environment, mount or URL overrides from model requests. Do not expose an
unauthenticated endpoint to other users or networks. Its evidence is only as trusted
as the host configuration, pinned image, implementation and protected record store.

Run offline tests with `python3 -m unittest -v` in this directory. Live container
checks require an authorized pinned reference image and a functioning container
engine; they are deliberately not silently launched by the unit tests.
