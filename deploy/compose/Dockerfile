# One Dockerfile, four binaries.
#
# Not three files. `compose.yaml` used to name `Dockerfile.gateway`,
# `.control` and `.ingester`, and three files would mean three independent
# builds of the same workspace — the same dependency graph compiled three
# times, and three chances for one image to be built from a different commit
# than the two it talks to. A shared `builder` stage compiles once; the three
# runtime stages copy a binary each out of it, and `target:` in compose picks
# which.
#
# `ancre-verify` gets a stage too, even though it is not a service. An auditor
# is not required to own a Rust toolchain, and the quickstart's last step is
# verification — so the verifier has to be runnable by anyone who can run
# `docker compose`.

# ---------------------------------------------------------------------------

FROM rust:1.93-slim-bookworm AS builder

# rustls everywhere, so there is no OpenSSL to link against — this is the whole
# list, and keeping it this short is a security-questionnaire answer as much as
# a build-time one (PRD §10).
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY bench ./bench

# The commit that produced these binaries, stamped into `gateway_version` and
# from there into every pin. Passed in rather than read from `.git`, which is
# not in the build context — an image is a thing you ship, and shipping the
# repository's history inside it to compute one string is the wrong trade.
#
# Unset means the literal `unknown`, which is the honest answer: a pin that
# cannot name its build must not pretend to. `quickstart.sh` exports it.
ARG ANCRE_BUILD_SHA=unknown
ENV ANCRE_BUILD_SHA=${ANCRE_BUILD_SHA}

# `--locked`, because an image built from a resolved-on-the-fly dependency
# graph is not the artefact that was tested. The cache mounts make a rebuild
# cheap; they are also why the binaries are copied out inside the same `RUN` —
# a cache mount is not part of the resulting layer, so anything left in
# `/build/target` when this step ends is gone.
#
# ## Why the `touch`
#
# `COPY` preserves each file's mtime from the build context, and cargo decides
# what to rebuild by comparing source mtimes against cached artefacts. With a
# persistent `/build/target`, a source file edited *before* the previous image
# was built — but copied into this one *after* — looks older than the artefact
# compiled from its earlier contents, so cargo declares it fresh and the image
# ships a binary that does not match its source.
#
# That is not a theoretical race. It happened during this file's own
# development: an edit to `ancre-types` was silently dropped, and the build
# failed several crates downstream with an error that pointed at the wrong
# problem. A build cache that can serve stale code is worse than no cache,
# because the artefact it produces is not the artefact anyone reviewed.
#
# Touching the workspace sources costs a recompile of nine small crates and
# keeps every third-party dependency cached, which is where the minutes are.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    find crates bench Cargo.toml Cargo.lock -type f -exec touch {} + \
 && cargo build --release --locked \
      -p ancre-gateway -p ancre-control -p ancre-ingester -p ancre-verify \
 && mkdir -p /out \
 && cp target/release/ancre-gateway \
       target/release/ancre-control \
       target/release/ancre-ingester \
       target/release/ancre-verify /out/

# ---------------------------------------------------------------------------

# Shared runtime. `ca-certificates` is not optional: without it every TLS
# handshake to a provider fails, and the failure surfaces as an upstream error
# on the customer's first real request rather than at boot.
FROM debian:bookworm-slim AS runtime
#
# `curl` is here for the compose healthchecks and nothing else. A container
# that cannot answer "are you ready" gets traffic sent to it during a deploy,
# and the alternative — an image with no way to probe itself — trades a
# megabyte for a class of startup race that is miserable to diagnose.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home --home-dir /var/lib/ancre ancre \
 && mkdir -p /var/lib/ancre/keys \
 && chown -R ancre:ancre /var/lib/ancre
USER ancre
WORKDIR /var/lib/ancre

# ---------------------------------------------------------------------------

FROM runtime AS gateway
COPY --from=builder /out/ancre-gateway /usr/local/bin/
EXPOSE 8080
ENTRYPOINT ["ancre-gateway"]

FROM runtime AS control
COPY --from=builder /out/ancre-control /usr/local/bin/
# The verifier rides along here on purpose. The control plane is what serves
# the chain, the checkpoints and the keys, so it is the one container where
# `docker compose exec` can verify evidence without leaving the deployment.
COPY --from=builder /out/ancre-verify /usr/local/bin/
EXPOSE 8081
ENTRYPOINT ["ancre-control"]

FROM runtime AS ingester
COPY --from=builder /out/ancre-ingester /usr/local/bin/
ENTRYPOINT ["ancre-ingester"]

# The demo provider. A separate stage inheriting `builder`, so it is compiled
# only when something asks for this target — the three service images do not
# depend on it and cannot accidentally carry it.
FROM builder AS mock-builder
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    find crates -type f -exec touch {} + \
 && cargo build --release --locked -p ancre-gateway --example mock-provider \
 && cp target/release/examples/mock-provider /out/

FROM runtime AS mock
COPY --from=mock-builder /out/mock-provider /usr/local/bin/
EXPOSE 9090
ENTRYPOINT ["mock-provider"]

# Reachable as `docker compose run --rm verify`, and useful on its own:
# `docker run --rm -i ancre-verify --chain - < chain.jsonl` verifies a chain on
# a machine with no toolchain and no network.
FROM runtime AS verify
COPY --from=builder /out/ancre-verify /usr/local/bin/
ENTRYPOINT ["ancre-verify"]
