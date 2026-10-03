# midir container image (published to ghcr.io by .github/workflows/release.yml; build context: repository root).
# Dependencies come from pyproject.toml + uv.lock and are installed in their own layer, so code edits do not
# reinstall them. Runtime files are read from the working directory /app: .env values arrive as environment variables
# (compose env_file) and config/midir.toml as a compose config.
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_NATIVE_TLS=1 PYTHONUNBUFFERED=1 PATH="/app/.venv/bin:$PATH"
WORKDIR /app
COPY pyproject.toml uv.lock README.md LICENSE ./
RUN uv sync --frozen --no-dev --no-install-project
COPY src ./src
RUN uv sync --frozen --no-dev --no-editable && mkdir -p /app/config
# no root at runtime; compose overrides the ids with yours (user: "${UID}:${GID}")
USER 1000:1000
EXPOSE 18880
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=5 \
  CMD ["python", "-c", "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:18880/health', timeout=2).status == 200 else 1)"]
CMD ["midir", "--host", "0.0.0.0", "--port", "18880"]
LABEL org.opencontainers.image.title="midir" \
      org.opencontainers.image.description="Midir: OpenAI- and Anthropic-compatible gateway for LLM backends (StackSpot AI agents today)"
