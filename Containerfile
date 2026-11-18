# Named Containerfile because Podman looks for that first and Docker accepts it with -f; nothing here is
# specific to either.
#
# The build context is a directory of finished binaries, one per platform:
#
#     linux/amd64/heliobridge      already mode 0755, since COPY preserves what it is given
#     linux/arm64/heliobridge
#
# plus LICENSE, NOTICE and an empty rootfs/state/ at its root, which the release workflow creates. A local
# build has to create it in its context too: `mkdir -p rootfs/state`.
#
# So this file only copies. Nothing executes during the build, which is why `--platform
# linux/amd64,linux/arm64` needs no QEMU. Compiling inside the image would need emulation for the
# non-native architecture and take an order of magnitude longer.
FROM scratch

# Provided by buildx. Declared so COPY can use it to pick the matching binary.
ARG TARGETPLATFORM

# The whole image. A statically linked musl binary needs no libc, no shell, and no CA bundle either — the
# TLS roots are compiled in, and a private authority is supplied by mounting it and setting SSL_CERT_FILE.
# So there is nothing here with a CVE feed and nothing to shell into.
COPY ${TARGETPLATFORM}/heliobridge /heliobridge

# Apache-2.0 requires both to travel with the work.
COPY LICENSE NOTICE /

# The user the process runs as, and the owner of /state. Numeric because there is no /etc/passwd to
# resolve a name against. 65532 is the conventional "nonroot" uid, so it matches what people already grant
# on a volume; a build for a host that grants something else passes --build-arg UID=… --build-arg GID=….
ARG UID=65532
ARG GID=65532

# The mount point for the state volume, owned by that user. Docker gives a new named volume the
# ownership of the directory it is mounted over, and without one in the image that is root, which this
# user cannot write to. Copied because a scratch image has nothing to run mkdir with, and copied as the
# parent directory so that /state itself takes the owner rather than only what it contains.
COPY --chown=${UID}:${GID} rootfs/ /

USER ${UID}:${GID}

# Where the device connects. Above 1024, so this needs no capability and no root.
EXPOSE 7006

# Deliberately no VOLUME: it would create anonymous volumes on `docker run` and surprise people. The
# README says which path to mount.

# Reports whether the server is serving, not whether a device is connected: one is expected to be absent
# for hours, and a container is not broken while it sleeps.
#
# start-period covers certificate generation on a first run with an empty state volume.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/heliobridge", "healthz"]

ENTRYPOINT ["/heliobridge"]
