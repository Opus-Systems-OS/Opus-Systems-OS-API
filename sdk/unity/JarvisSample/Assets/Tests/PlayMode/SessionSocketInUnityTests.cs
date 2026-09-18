using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Text;
using NUnit.Framework;
using OpusSystems.Api;
using UnityEngine;
using UnityEngine.TestTools;

/// <summary>
/// The headset path under Unity's runtime: ClientWebSocket via SessionSocket,
/// deltas streaming in, the authoritative event after. Needs OPUS_API_KEY and
/// OPUS_LIVE_SESSION=1 (it spends a few cents on jarvis).
///   Unity -batchmode -projectPath . -runTests -testPlatform PlayMode -testResults results.xml
/// </summary>
public class SessionSocketInUnityTests
{
    [UnityTest]
    public IEnumerator JarvisTurnStreamsOverTheWebSocket()
    {
        var key = Environment.GetEnvironmentVariable("OPUS_API_KEY");
        if (string.IsNullOrEmpty(key) || Environment.GetEnvironmentVariable("OPUS_LIVE_SESSION") != "1")
        {
            Assert.Ignore("OPUS_API_KEY / OPUS_LIVE_SESSION not set");
            yield break;
        }

        var api = new OpusClient("https://api.opustower.dev/v1", key);
        var deltas = new StringBuilder();
        string reply = null, stop = null, error = null;
        var queue = new ConcurrentQueue<Action>();

        var create = api.CreateSessionAsync(new CreateSessionRequest
        {
            AgentSlug = "jarvis",
            Task = "Reply with one short sentence: what is 23 times 3?",
            SystemSuffix = "You are being tested from a Unity runtime.",
        });
        while (!create.IsCompleted) yield return null;
        if (create.Exception != null) throw create.Exception.InnerException ?? create.Exception;

        var open = api.OpenSessionSocketAsync(create.Result.SessionId, history: false, deltas: true);
        while (!open.IsCompleted) yield return null;
        if (open.Exception != null) throw open.Exception.InnerException ?? open.Exception;
        var ws = open.Result;
        ws.OnDelta += (_, text) => queue.Enqueue(() => deltas.Append(text));
        ws.OnEvent += ev => queue.Enqueue(() =>
        {
            if (Events.Type(ev) == "agent.message") reply = Events.Text(ev);
            if (Events.Type(ev) == "session.status_idle") stop = Events.StopReason(ev);
        });
        ws.OnError += (t, m) => queue.Enqueue(() => error = t + ": " + m);

        // The create already sent the task; the socket opened after, so
        // ask again on the socket to get a turn we watch from the start.
        var send = ws.SendAsync("Once more, one short sentence: what is 23 times 3?");
        while (!send.IsCompleted) yield return null;

        var deadline = Time.realtimeSinceStartup + 90f;
        while (stop == null && error == null && Time.realtimeSinceStartup < deadline)
        {
            while (queue.TryDequeue(out var a)) a();
            yield return null;
        }
        while (queue.TryDequeue(out var a2)) a2();

        Assert.IsNull(error, error);
        Assert.AreEqual("end_turn", stop, "turn should end");
        StringAssert.Contains("69", reply ?? "", "reply");
        Assert.IsTrue(deltas.Length > 0, "deltas streamed");
        Debug.Log($"deltas: {deltas} | reply: {reply}");

        var close = ws.CloseAsync();
        while (!close.IsCompleted) yield return null;
        ws.Dispose();
        api.Dispose();
    }
}
