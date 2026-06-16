# Assembles a per-arch scratch image from a binary that CI has already
# cross-compiled (static musl, no libc to ship). buildx sets TARGETOS/TARGETARCH
# per --platform, and we pick the matching binary from the build context.
FROM scratch

ARG TARGETOS
ARG TARGETARCH
COPY dist/${TARGETOS}/${TARGETARCH}/evc04-charge /evc04-charge

ENTRYPOINT ["/evc04-charge"]
