"""mitmproxy addon: stream Server-Sent Events through instead of buffering them.

By default mitmproxy holds a whole response body before passing it on, which turns an SSE stream into one burst at the
end: tokens, tool calls and Midir's keepalives all arrive when generation is over. This addon streams every
`text/event-stream` response chunk by chunk, in both directions of the observability stack (clients -> Midir on the
reverse proxy, and Midir -> StackSpot on the regular proxy), while keeping a copy so mitmweb still shows the body.
"""
from mitmproxy import http


def responseheaders(flow: http.HTTPFlow) -> None:
    if flow.response and flow.response.headers.get("content-type", "").startswith("text/event-stream"):
        chunks: list[bytes] = []

        def stream(data: bytes) -> bytes:
            chunks.append(data)  # a copy for mitmweb; the bytes go out unchanged, right away
            return data

        flow.response.stream = stream
        flow.metadata["midir_sse_chunks"] = chunks


def response(flow: http.HTTPFlow) -> None:
    chunks = flow.metadata.pop("midir_sse_chunks", None)
    if chunks is not None and flow.response is not None:
        flow.response.stream = False
        flow.response.content = b"".join(chunks)  # for display only: the client already received every chunk
