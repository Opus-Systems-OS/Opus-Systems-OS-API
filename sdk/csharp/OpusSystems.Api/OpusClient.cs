using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace OpusSystems.Api
{
    /// <summary>
    /// The REST half of the API. One instance per key; thread-safe. Every
    /// non-2xx throws <see cref="OpusApiException"/> with the envelope.
    /// <code>
    /// var api = new OpusClient("https://api.opustower.dev/v1", "osk_…");
    /// var me = await api.MeAsync();
    /// var s = await api.CreateSessionAsync(new CreateSessionRequest { AgentSlug = "jarvis", Task = "Hey Jarvis" });
    /// using var ws = await api.OpenSessionSocketAsync(s.SessionId);
    /// </code>
    /// </summary>
    public sealed class OpusClient : IDisposable
    {
        public Uri BaseUrl { get; }
        private readonly string _key;
        private readonly HttpClient _http;
        private readonly bool _ownsHttp;

        public OpusClient(string baseUrl, string apiKey, HttpClient? http = null)
        {
            if (string.IsNullOrWhiteSpace(baseUrl)) throw new ArgumentException("baseUrl", nameof(baseUrl));
            // An empty key is allowed for a device that has none yet: only the
            // pairing calls work without one.
            if (!string.IsNullOrEmpty(apiKey) && !apiKey.StartsWith("osk_", StringComparison.Ordinal))
                throw new ArgumentException("apiKey must be an osk_… key, or empty until paired", nameof(apiKey));
            BaseUrl = new Uri(baseUrl.TrimEnd('/') + "/");
            _key = apiKey ?? "";
            _ownsHttp = http == null;
            _http = http ?? new HttpClient { Timeout = TimeSpan.FromSeconds(30) };
            _http.DefaultRequestHeaders.UserAgent.ParseAdd("opus-systems-api-csharp/0.1.0");
        }

        // ---- auth --------------------------------------------------------

        public Task<Me> MeAsync(CancellationToken ct = default) => GetAsync<Me>("me", ct);

        // ---- fleet -------------------------------------------------------

        public async Task<List<Agent>> AgentsAsync(CancellationToken ct = default)
            => (await GetAsync<Page<Agent>>("fleet/agents", ct).ConfigureAwait(false)).Data;

        public Task<Rig> RigAsync(CancellationToken ct = default) => GetAsync<Rig>("rig", ct);

        // ---- sessions ----------------------------------------------------

        public Task<CreatedSession> CreateSessionAsync(CreateSessionRequest req, CancellationToken ct = default)
            => PostAsync<CreatedSession>("sessions", req, ct);

        public Task<Page<JObject>> ListSessionsAsync(string? agentSlug = null, int? limit = null, string? page = null, CancellationToken ct = default)
        {
            var q = new List<string>();
            if (agentSlug != null) q.Add("agent_slug=" + Uri.EscapeDataString(agentSlug));
            if (limit != null) q.Add("limit=" + limit);
            if (page != null) q.Add("page=" + Uri.EscapeDataString(page));
            return GetAsync<Page<JObject>>("sessions" + (q.Count > 0 ? "?" + string.Join("&", q) : ""), ct);
        }

        /// <summary>The Managed Agents session object plus console_url.</summary>
        public Task<JObject> GetSessionAsync(string sessionId, CancellationToken ct = default)
            => GetAsync<JObject>("sessions/" + Id(sessionId), ct);

        /// <summary>
        /// Event history. <paramref name="types"/> is comma-separated
        /// ("agent.message,session.error"); <paramref name="descending"/>
        /// with <paramref name="limit"/> 1 is "the latest".
        /// </summary>
        public Task<Page<JObject>> EventsAsync(string sessionId, string? types = null, int? limit = null, bool descending = false, string? page = null, CancellationToken ct = default)
        {
            var q = new List<string>();
            if (types != null) q.Add("types=" + Uri.EscapeDataString(types));
            if (limit != null) q.Add("limit=" + limit);
            if (descending) q.Add("order=desc");
            if (page != null) q.Add("page=" + Uri.EscapeDataString(page));
            return GetAsync<Page<JObject>>("sessions/" + Id(sessionId) + "/events" + (q.Count > 0 ? "?" + string.Join("&", q) : ""), ct);
        }

        /// <summary>The session's latest agent.message text, or null.</summary>
        public async Task<string?> LastReplyAsync(string sessionId, CancellationToken ct = default)
        {
            var page = await EventsAsync(sessionId, "agent.message", 1, descending: true, ct: ct).ConfigureAwait(false);
            return page.Data.Count == 0 ? null : Events.Text(page.Data[0]);
        }

        /// <summary>A follow-up user message; resumes an idle session.</summary>
        public Task<Page<JObject>> SendAsync(string sessionId, string task, CancellationToken ct = default)
            => PostAsync<Page<JObject>>("sessions/" + Id(sessionId) + "/events", new { task }, ct);

        /// <summary>Answer agent.custom_tool_use events; the waiting turn continues.</summary>
        public Task<Page<JObject>> SendToolResultsAsync(string sessionId, IEnumerable<ToolResult> results, CancellationToken ct = default)
            => PostAsync<Page<JObject>>("sessions/" + Id(sessionId) + "/tool-results", new { results = new List<ToolResult>(results) }, ct);

        public Task<Page<JObject>> InterruptAsync(string sessionId, CancellationToken ct = default)
            => PostAsync<Page<JObject>>("sessions/" + Id(sessionId) + "/interrupt", null, ct);

        /// <summary>Open the session's WebSocket. See <see cref="SessionSocket"/>.</summary>
        /// <param name="deltas">Also receive <see cref="SessionSocket.OnDelta"/> text fragments as replies are generated.</param>
        public Task<SessionSocket> OpenSessionSocketAsync(string sessionId, bool history = true, bool deltas = false, CancellationToken ct = default)
        {
            var q = new List<string>();
            if (!history) q.Add("history=false");
            if (deltas) q.Add("deltas=true");
            var b = new UriBuilder(new Uri(BaseUrl, "sessions/" + Id(sessionId) + "/ws"))
            {
                Scheme = BaseUrl.Scheme == "https" ? "wss" : "ws",
                Query = string.Join("&", q),
            };
            return SessionSocket.ConnectAsync(b.Uri, _key, ct);
        }

        // ---- usage -------------------------------------------------------

        public Task<Usage> UsageAsync(string? since = null, string? until = null, CancellationToken ct = default)
        {
            var q = new List<string>();
            if (since != null) q.Add("since=" + Uri.EscapeDataString(since));
            if (until != null) q.Add("until=" + Uri.EscapeDataString(until));
            return GetAsync<Usage>("usage" + (q.Count > 0 ? "?" + string.Join("&", q) : ""), ct);
        }

        // ---- inference ---------------------------------------------------

        /// <summary>Ollama's /api/tags. Throws rig_offline (503) when the rig is off.</summary>
        public Task<JObject> InferenceModelsAsync(CancellationToken ct = default) => GetAsync<JObject>("inference/models", ct);

        /// <summary>
        /// One buffered chat completion on the rig (stream:false). <paramref name="body"/>
        /// is Ollama's /api/chat body: {model, messages, options?, …}.
        /// </summary>
        public Task<JObject> InferenceChatAsync(JObject body, CancellationToken ct = default)
        {
            body["stream"] = false;
            return PostAsync<JObject>("inference/chat", body, ct);
        }

        public Task<JObject> InferenceEmbeddingsAsync(string model, string input, CancellationToken ct = default)
            => PostAsync<JObject>("inference/embeddings", new { model, input }, ct);

        // ---- voice -------------------------------------------------------

        /// <summary>Jarvis says <paramref name="text"/>: the audio bytes (mp3 by default). Needs the voice scope.</summary>
        public async Task<byte[]> SpeakAsync(string text, string format = "mp3", string latency = "low", CancellationToken ct = default)
        {
            using var req = new HttpRequestMessage(HttpMethod.Post, new Uri(BaseUrl, "voice/speak"));
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _key);
            req.Content = new StringContent(JsonConvert.SerializeObject(new { text, format, latency }), Encoding.UTF8, "application/json");
            using var res = await _http.SendAsync(req, ct).ConfigureAwait(false);
            if (!res.IsSuccessStatusCode)
                throw Envelope(res, await res.Content.ReadAsStringAsync().ConfigureAwait(false));
            return await res.Content.ReadAsByteArrayAsync().ConfigureAwait(false);
        }

        /// <summary>Which voice the API speaks with; throws not_found when voice is not configured.</summary>
        public Task<VoiceInfo> VoiceInfoAsync(CancellationToken ct = default) => GetAsync<VoiceInfo>("voice", ct);

        // ---- pairing (no key) ----------------------------------------------

        /// <summary>Start pairing this device: show <c>code</c>, keep <c>token</c>. Works with an empty key.</summary>
        public async Task<JObject> PairStartAsync(CancellationToken ct = default)
        {
            using var req = new HttpRequestMessage(HttpMethod.Post, new Uri(BaseUrl, "pair"));
            using var res = await _http.SendAsync(req, ct).ConfigureAwait(false);
            var text = await res.Content.ReadAsStringAsync().ConfigureAwait(false);
            if (!res.IsSuccessStatusCode) throw Envelope(res, text);
            return JObject.Parse(text);
        }

        /// <summary>Poll a pairing: null while pending; the bundle {api_key, key_id, name, wit_token?, speaker?} once approved.</summary>
        public async Task<JObject?> PairPollAsync(string code, string token, CancellationToken ct = default)
        {
            using var req = new HttpRequestMessage(HttpMethod.Get, new Uri(BaseUrl, "pair/" + Uri.EscapeDataString(code) + "?token=" + Uri.EscapeDataString(token)));
            using var res = await _http.SendAsync(req, ct).ConfigureAwait(false);
            if ((int)res.StatusCode == 202) return null;
            var text = await res.Content.ReadAsStringAsync().ConfigureAwait(false);
            if (!res.IsSuccessStatusCode) throw Envelope(res, text);
            return JObject.Parse(text);
        }

        // ---- ops (ops:read) ------------------------------------------------

        /// <summary>The stack's services at a glance: <c>{services:[{id,name,state,headline,checked_at}]}</c>.</summary>
        public Task<JObject> OpsAsync(CancellationToken ct = default) => GetAsync<JObject>("ops", ct);

        /// <summary>One service with its <c>detail</c> document: github, uptimerobot, droplet, docker, tailscale, cloudflare.</summary>
        public Task<JObject> OpsAsync(string service, CancellationToken ct = default) => GetAsync<JObject>("ops/" + Uri.EscapeDataString(service), ct);

        // ---- plumbing ----------------------------------------------------

        private static string Id(string id)
        {
            foreach (var c in id)
                if (!(char.IsLetterOrDigit(c) || c == '_' || c == '-'))
                    throw new ArgumentException("id has unexpected characters", nameof(id));
            return id;
        }

        private async Task<T> GetAsync<T>(string path, CancellationToken ct)
        {
            using var req = new HttpRequestMessage(HttpMethod.Get, new Uri(BaseUrl, path));
            return await SendAsync<T>(req, ct).ConfigureAwait(false);
        }

        private async Task<T> PostAsync<T>(string path, object? body, CancellationToken ct)
        {
            using var req = new HttpRequestMessage(HttpMethod.Post, new Uri(BaseUrl, path));
            if (body != null)
                req.Content = new StringContent(JsonConvert.SerializeObject(body), Encoding.UTF8, "application/json");
            return await SendAsync<T>(req, ct).ConfigureAwait(false);
        }

        private async Task<T> SendAsync<T>(HttpRequestMessage req, CancellationToken ct)
        {
            if (_key.Length == 0) throw new OpusApiException(HttpStatusCode.Unauthorized, "unauthorized", "this client has no API key yet — pair first", "", null);
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _key);
            using var res = await _http.SendAsync(req, ct).ConfigureAwait(false);
            var text = await res.Content.ReadAsStringAsync().ConfigureAwait(false);
            if (!res.IsSuccessStatusCode) throw Envelope(res, text);
            return JsonConvert.DeserializeObject<T>(text)
                   ?? throw new OpusApiException(res.StatusCode, "internal", "empty response body", RequestId(res), null);
        }

        /// <summary>Parse a non-2xx response into the typed exception. Public for tests and custom transports.</summary>
        public static OpusApiException Envelope(HttpResponseMessage res, string text)
        {
            string type = "internal", message = text, requestId = RequestId(res);
            try
            {
                var err = JObject.Parse(text)["error"];
                if (err != null)
                {
                    type = err.Value<string>("type") ?? type;
                    message = err.Value<string>("message") ?? message;
                    requestId = err.Value<string>("request_id") ?? requestId;
                }
            }
            catch (JsonException) { /* not the envelope; keep the raw text */ }
            int? retry = null;
            if (res.Headers.RetryAfter?.Delta is TimeSpan d) retry = (int)Math.Ceiling(d.TotalSeconds);
            return new OpusApiException(res.StatusCode, type, message, requestId, retry);
        }

        private static string RequestId(HttpResponseMessage res)
            => res.Headers.TryGetValues("x-request-id", out var v) ? string.Join("", v) : "";

        public void Dispose()
        {
            if (_ownsHttp) _http.Dispose();
        }
    }
}
