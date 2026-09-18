using System.Net;
using System.Net.Http;
using Newtonsoft.Json.Linq;
using OpusSystems.Api;
using Xunit;

public class UnitTests
{
    [Fact]
    public void EnvelopeBecomesTypedException()
    {
        var res = new HttpResponseMessage(HttpStatusCode.ServiceUnavailable);
        res.Headers.Add("x-request-id", "req_abc");
        res.Headers.Add("retry-after", "5");
        var ex = OpusClient.Envelope(res,
            "{\"error\":{\"type\":\"rig_offline\",\"message\":\"rig offline: deadline\",\"request_id\":\"req_abc\"}}");
        Assert.Equal(HttpStatusCode.ServiceUnavailable, ex.Status);
        Assert.Equal("rig_offline", ex.Type);
        Assert.True(ex.IsRigOffline);
        Assert.Equal("req_abc", ex.RequestId);
        Assert.Equal(5, ex.RetryAfterSeconds);
        Assert.Contains("503 rig_offline", ex.Message);
    }

    [Fact]
    public void NonEnvelopeBodyIsKeptAsMessage()
    {
        var res = new HttpResponseMessage(HttpStatusCode.BadGateway);
        var ex = OpusClient.Envelope(res, "<html>caddy</html>");
        Assert.Equal("internal", ex.Type);
        Assert.Contains("<html>caddy</html>", ex.Message);
        Assert.Null(ex.RetryAfterSeconds);
    }

    [Fact]
    public void EventHelpersReadAnthropicShapes()
    {
        var msg = JObject.Parse("{\"id\":\"sevt_1\",\"type\":\"agent.message\",\"content\":[{\"type\":\"text\",\"text\":\"17 times \"},{\"type\":\"text\",\"text\":\"23 is 391.\"}]}");
        Assert.Equal("agent.message", Events.Type(msg));
        Assert.Equal("sevt_1", Events.Id(msg));
        Assert.Equal("17 times 23 is 391.", Events.Text(msg));
        Assert.Null(Events.StopReason(msg));

        var idle = JObject.Parse("{\"type\":\"session.status_idle\",\"stop_reason\":{\"type\":\"end_turn\"}}");
        Assert.Equal("end_turn", Events.StopReason(idle));
        Assert.Equal("", Events.Text(idle));

        var err = JObject.Parse("{\"type\":\"session.error\",\"error\":{\"type\":\"billing_error\",\"message\":\"credit balance is too low\"}}");
        Assert.Equal("billing_error: credit balance is too low", Events.Error(err));
    }

    [Fact]
    public async Task ClientRejectsBadInputsBeforeAnyRequest()
    {
        Assert.Throws<ArgumentException>(() => new OpusClient("https://api.opustower.dev/v1", "not-a-key"));
        using var api = new OpusClient("https://api.opustower.dev/v1/", "osk_00000000_" + new string('0', 64));
        Assert.Equal("https://api.opustower.dev/v1/", api.BaseUrl.ToString());
        await Assert.ThrowsAsync<ArgumentException>(() => api.GetSessionAsync("../keys"));
    }

    [Fact]
    public void CustomToolAndResultSerializeToTheWireShape()
    {
        var req = new CreateSessionRequest
        {
            AgentSlug = "jarvis",
            Task = "play something",
            Tools = new List<CustomTool> { new CustomTool { Name = "play_music", Description = "Play music on this device." } },
            SystemSuffix = "Call me Sir.",
        };
        var json = JObject.Parse(Newtonsoft.Json.JsonConvert.SerializeObject(req));
        Assert.Equal("custom", json["tools"]![0]!["type"]);
        Assert.Equal("object", json["tools"]![0]!["input_schema"]!["type"]);
        Assert.Equal("Call me Sir.", json["system_suffix"]);
        Assert.Null(json["environment"]);

        var r = JObject.Parse(Newtonsoft.Json.JsonConvert.SerializeObject(new ToolResult { CustomToolUseId = "sevt_1", Content = "ok" }));
        Assert.Equal("sevt_1", r["custom_tool_use_id"]);
        Assert.Null(r["is_error"]);

        var use = JObject.Parse("{\"id\":\"sevt_7\",\"type\":\"agent.custom_tool_use\",\"name\":\"play_music\",\"input\":{\"artist\":\"Daft Punk\"}}");
        Assert.Equal("play_music", Events.ToolName(use));
        Assert.Equal("Daft Punk", Events.ToolInput(use)["artist"]);
        var idle = JObject.Parse("{\"type\":\"session.status_idle\",\"stop_reason\":{\"type\":\"requires_action\",\"event_ids\":[\"sevt_7\"]}}");
        Assert.True(Events.RequiresAction(idle));
    }
}
