using System;
using System.IO;
using System.Net.WebSockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;

namespace OpusSystems.Api
{
    /// <summary>
    /// The session WebSocket (<c>GET /v1/sessions/{id}/ws</c>). Frames are
    /// JSON objects with a <c>type</c>; this class turns them into events
    /// and turns <see cref="SendAsync"/> / <see cref="InterruptAsync"/> into
    /// frames. Handlers run on the receive loop's thread — marshal to the
    /// main thread yourself in Unity.
    /// <code>
    /// ws.OnEvent += ev => { if (Events.Type(ev) == "agent.message") Say(Events.Text(ev)); };
    /// await ws.SendAsync("What's the weather like on Mars?");
    /// </code>
    /// </summary>
    public sealed class SessionSocket : IDisposable
    {
        private readonly ClientWebSocket _ws;
        private readonly CancellationTokenSource _cts = new CancellationTokenSource();
        private readonly SemaphoreSlim _sendLock = new SemaphoreSlim(1, 1);
        private Task? _loop;

        public string SessionId { get; private set; } = "";
        /// <summary>The request id of the upgrade — quote it when reporting a problem.</summary>
        public string RequestId { get; private set; } = "";
        public bool IsOpen => _ws.State == WebSocketState.Open;

        /// <summary>A session event: the same object /events carries. Use <see cref="Events"/> to read it.</summary>
        public event Action<JObject>? OnEvent;
        /// <summary>A client frame was rejected; the socket stays open.</summary>
        public event Action<string, string>? OnError;
        /// <summary>Answer to a message/tool_result/interrupt: the events appended.</summary>
        public event Action<JArray>? OnSent;
        /// <summary>With deltas enabled: (event id, text fragment) as a reply is generated. Speak these; store the event.</summary>
        public event Action<string, string>? OnDelta;
        /// <summary>The server closed: reason is "upstream_closed" (session ended) or a transport error.</summary>
        public event Action<string>? OnClosed;

        private SessionSocket(ClientWebSocket ws) { _ws = ws; }

        internal static async Task<SessionSocket> ConnectAsync(Uri url, string apiKey, CancellationToken ct)
        {
            var ws = new ClientWebSocket();
            ws.Options.SetRequestHeader("Authorization", "Bearer " + apiKey);
            try
            {
                await ws.ConnectAsync(url, ct).ConfigureAwait(false);
            }
            catch (WebSocketException e)
            {
                // A refused upgrade is an ordinary HTTP error; surface it as such.
                ws.Dispose();
                throw new OpusApiException(System.Net.HttpStatusCode.BadGateway, "upstream",
                    "websocket upgrade refused: " + e.Message, "", null);
            }
            var s = new SessionSocket(ws);
            var hello = await s.ReadFrameAsync(ct).ConfigureAwait(false)
                        ?? throw new OpusApiException(System.Net.HttpStatusCode.BadGateway, "upstream", "socket closed before hello", "", null);
            if (hello.Value<string>("type") != "hello")
                throw new OpusApiException(System.Net.HttpStatusCode.BadGateway, "upstream", "expected hello, got " + hello, "", null);
            s.SessionId = hello.Value<string>("session_id") ?? "";
            s.RequestId = hello.Value<string>("request_id") ?? "";
            s._loop = Task.Run(s.ReceiveLoopAsync);
            return s;
        }

        /// <summary>A follow-up user message (needs sessions:write).</summary>
        public Task SendAsync(string task, CancellationToken ct = default)
            => SendFrameAsync(new JObject { ["type"] = "message", ["task"] = task }, ct);

        /// <summary>Answer an agent.custom_tool_use event (needs sessions:write).</summary>
        public Task SendToolResultAsync(string customToolUseId, string content, bool isError = false, CancellationToken ct = default)
            => SendFrameAsync(new JObject
            {
                ["type"] = "tool_result",
                ["custom_tool_use_id"] = customToolUseId,
                ["content"] = content,
                ["is_error"] = isError,
            }, ct);

        public Task InterruptAsync(CancellationToken ct = default)
            => SendFrameAsync(new JObject { ["type"] = "interrupt" }, ct);

        public Task PingAsync(CancellationToken ct = default)
            => SendFrameAsync(new JObject { ["type"] = "ping" }, ct);

        public async Task CloseAsync(CancellationToken ct = default)
        {
            _cts.Cancel();
            if (_ws.State == WebSocketState.Open)
            {
                try { await _ws.CloseAsync(WebSocketCloseStatus.NormalClosure, "bye", ct).ConfigureAwait(false); }
                catch (WebSocketException) { }
                catch (OperationCanceledException) { }
            }
        }

        private async Task SendFrameAsync(JObject frame, CancellationToken ct)
        {
            var bytes = Encoding.UTF8.GetBytes(frame.ToString(Newtonsoft.Json.Formatting.None));
            await _sendLock.WaitAsync(ct).ConfigureAwait(false);
            try
            {
                await _ws.SendAsync(new ArraySegment<byte>(bytes), WebSocketMessageType.Text, true, ct).ConfigureAwait(false);
            }
            finally { _sendLock.Release(); }
        }

        private async Task ReceiveLoopAsync()
        {
            string reason = "closed";
            try
            {
                while (!_cts.IsCancellationRequested)
                {
                    var frame = await ReadFrameAsync(_cts.Token).ConfigureAwait(false);
                    if (frame == null) break;
                    switch (frame.Value<string>("type"))
                    {
                        case "event":
                            if (frame["event"] is JObject ev) OnEvent?.Invoke(ev);
                            break;
                        case "sent":
                            OnSent?.Invoke(frame["data"] as JArray ?? new JArray());
                            break;
                        case "delta":
                            OnDelta?.Invoke(frame.Value<string>("event_id") ?? "", frame.Value<string>("text") ?? "");
                            break;
                        case "error":
                            OnError?.Invoke(frame["error"]?.Value<string>("type") ?? "error",
                                            frame["error"]?.Value<string>("message") ?? "");
                            break;
                        case "closed":
                            reason = frame.Value<string>("reason") ?? "closed";
                            break;
                        case "pong":
                        default:
                            break;
                    }
                }
            }
            catch (OperationCanceledException) { reason = "client_closed"; }
            catch (WebSocketException e) { reason = "transport: " + e.Message; }
            OnClosed?.Invoke(reason);
        }

        /// <summary>One complete text frame as JSON, or null on close.</summary>
        private async Task<JObject?> ReadFrameAsync(CancellationToken ct)
        {
            var buf = new byte[16 * 1024];
            using var ms = new MemoryStream();
            while (true)
            {
                var r = await _ws.ReceiveAsync(new ArraySegment<byte>(buf), ct).ConfigureAwait(false);
                if (r.MessageType == WebSocketMessageType.Close) return null;
                ms.Write(buf, 0, r.Count);
                if (r.EndOfMessage) break;
            }
            return JObject.Parse(Encoding.UTF8.GetString(ms.ToArray()));
        }

        public void Dispose()
        {
            _cts.Cancel();
            _ws.Dispose();
            _cts.Dispose();
            _sendLock.Dispose();
        }
    }
}
