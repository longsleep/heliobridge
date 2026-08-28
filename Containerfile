# Named Containerfile because Podman looks for that first and Docker accepts it with -f; nothing here is
# specific to either.
#
# The build context is a directory of finished binaries, one per platform:
#
#     linux/amd64/heliobridge
#     linux/arm64/heliobridge
#
# so this file only copies. Nothing executes during the build, which is why `--platform
# linux/amd64,linux/arm64` needs no QEMU. Compiling inside the image would need emulation for the
# non-native architecture and take an order of magnitude longer.
FROM scratch

# Provided by buildx. Declared so COPY can use it to pick the matching binary.
ARG TARGETPLATFORM

# The whole image. A statically linked musl binary needs no libc, no shell, and no CA bundle either — the
# TLS roots are compiled in, and a private authority is supplied by mounting it and setting SSL_CERT_FILE.
# So there is nothing here with a CVE feed and nothing to shell into.
COPY ${TARGETPLATFORM}/heliobridge /heliobridge

# Numeric because there is no /etc/passwd to resolve a name against. 65532 is the conventional "nonroot"
# uid, so it matches what people already grant on a volume.
USER 65532:65532

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
