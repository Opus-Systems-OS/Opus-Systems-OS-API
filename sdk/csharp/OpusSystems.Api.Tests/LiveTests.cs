using Newtonsoft.Json.Linq;
using OpusSystems.Api;
using Xunit;

/// <summary>
/// Against the real API. Run with OPUS_API_KEY (and optionally OPUS_API_URL)
/// set; skipped otherwise. The session test spends a few cents on jarvis.
/// </summary>
public class LiveTests
{
    private static OpusClient? Client()
    {
        var key = Environment.GetEnvironmentVariable("OPUS_API_KEY");
        if (string.IsNullOrEmpty(key)) return null;
        var url = Environment.GetEnvironmentVariable("OPUS_API_URL") ?? "https://api.opustower.dev/v1";
        return new OpusClient(url, key);
    }

    [Fact]
    public async Task MeAgentsRigUsage()
    {
        using var api = Client();
        if (api == null) return;
        var me = await api.MeAsync();
        Assert.StartsWith("osk_", "osk_" + me.KeyId);
        Assert.Contains("fleet:read", me.Scopes);

        var agents = await api.AgentsAsync();
        Assert.Contains(agents, a => a.Slug == "jarvis");
        Assert.All(agents, a => Assert.Matches("^[0-9]+$", a.MaxListCostCents));

        var rig = await api.RigAsync();
        Assert.True(rig.Configured);
        if (!rig.Online) Assert.NotNull(rig.Reason);

        var usage = await api.UsageAsync();
        Assert.NotEmpty(usage.ByAgent);
    }

    [Fact]
    public async Task ErrorsAreTypedExceptions()
    {
        using var api = Client();
        if (api == null) return;
        // Anthropic validates the id's *format*, so a made-up id is
        // invalid_request (400), not not_found.
        var ex = await Assert.ThrowsAsync<OpusApiException>(() => api.GetSessionAsync("sesn_doesnotexist"));
        Assert.Equal("invalid_request", ex.Type);
        Assert.Equal(System.Net.HttpStatusCode.BadRequest, ex.Status);
        Assert.StartsWith("req_", ex.RequestId);
    }

    [Fact]
    public async Task JarvisTurnOverTheWebSocket()
    {
        using var api = Client();
        if (api == null || Environment.GetEnvironmentVariable("OPUS_LIVE_SESSION") != "1") return;

        var created = await api.CreateSessionAsync(new CreateSessionRequest
        {
            AgentSlug = "jarvis",
            Task = "Reply with one short sentence: what is 19 times 21?",
        });
        Assert.StartsWith("sesn_", created.SessionId);

        using var ws = await api.OpenSessionSocketAsync(created.SessionId, history: true);
        Assert.Equal(created.SessionId, ws.SessionId);
        var reply = new TaskCompletionSource<string>();
        var idle = new TaskCompletionSource<string>();
        ws.OnEvent += ev =>
        {
            switch (Events.Type(ev))
            {
                case "agent.message": reply.TrySetResult(Events.Text(ev)); break;
                case "session.status_idle": idle.TrySetResult(Events.StopReason(ev) ?? ""); break;
            }
        };
        ws.OnError += (t, m) => reply.TrySetException(new Exception(t + ": " + m));

        var text = await reply.Task.WaitAsync(TimeSpan.FromSeconds(90));
        Assert.Contains("399", text);
        Assert.Equal("end_turn", await idle.Task.WaitAsync(TimeSpan.FromSeconds(30)));

        var last = await api.LastReplyAsync(created.SessionId);
        Assert.Equal(text, last);
        await ws.CloseAsync();
    }
}
