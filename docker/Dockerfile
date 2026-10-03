# Midir container image (published to ghcr.io by .github/workflows; build context: repository root).
# Two stages: uv installs the locked dependencies and Midir into a virtual environment, and the final image carries
# only Python and that environment (no uv, no pip, no build files). Alpine keeps it small: about 80 MB, 30 MB to
# download. Runtime files come from the working directory /app: .env values arrive as environment variables (compose
# env_file) and config/midir.toml as a compose config.

FROM ghcr.io/astral-sh/uv:python3.12-alpine AS build
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never
WORKDIR /app
COPY pyproject.toml uv.lock README.md LICENSE ./
RUN --mount=type=cache,target=/root/.cache/uv uv sync --frozen --no-dev --no-install-project
COPY src ./src
RUN --mount=type=cache,target=/root/.cache/uv uv sync --frozen --no-dev --no-editable

FROM python:3.12-alpine
# pip and ensurepip are not needed at runtime: removing them does not shrink the image (they live in the base layer),
# but nothing can be installed into a running container
RUN rm -rf /usr/local/lib/python3.12/site-packages/pip* /usr/local/lib/python3.12/ensurepip /usr/local/bin/pip* \
 && mkdir -p /app/config
ENV PYTHONUNBUFFERED=1 PATH="/app/.venv/bin:$PATH"
WORKDIR /app
COPY --from=build /app/.venv /app/.venv
COPY LICENSE /app/LICENSE
# which build this is (midir --version, banner, /health): release (main), dev (develop), local (poe deploy-*).
# Last on purpose: commit and date change on every build, and nothing after them needs rebuilding.
ARG MIDIR_BUILD_CHANNEL=local
ARG MIDIR_BUILD_COMMIT=
ARG MIDIR_BUILD_DATE=
ENV MIDIR_BUILD_CHANNEL=$MIDIR_BUILD_CHANNEL MIDIR_BUILD_COMMIT=$MIDIR_BUILD_COMMIT MIDIR_BUILD_DATE=$MIDIR_BUILD_DATE
# no root at runtime; compose overrides the ids with yours (user: "${UID}:${GID}")
USER 1000:1000
EXPOSE 18880
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=5 \
  CMD ["python", "-c", "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:18880/health', timeout=2).status == 200 else 1)"]
CMD ["midir", "--host", "0.0.0.0", "--port", "18880"]
LABEL org.opencontainers.image.title="midir" \
      org.opencontainers.image.description="Midir: OpenAI- and Anthropic-compatible gateway for LLM backends (StackSpot AI agents today)" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/helton/midir" \
      org.opencontainers.image.revision="$MIDIR_BUILD_COMMIT"
