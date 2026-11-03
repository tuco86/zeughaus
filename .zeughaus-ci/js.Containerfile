# The image the omp plugin's jobs run in: Node (npm for publishing) and Bun,
# the runtime omp embeds.
FROM docker.io/library/node:26-trixie

COPY --from=docker.io/oven/bun:1.4.4 /usr/local/bin/bun /usr/local/bin/bun
