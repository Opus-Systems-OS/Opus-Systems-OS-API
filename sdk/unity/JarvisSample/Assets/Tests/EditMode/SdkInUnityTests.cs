using System;
using System.Collections;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using OpusSystems.Api;
using UnityEngine.TestTools;

/// <summary>
/// Proves the SDK compiles and behaves inside Unity's runtime (netstandard
/// 2.1 profile, Unity's Newtonsoft). The live test needs OPUS_API_KEY.
///   Unity -batchmode -projectPath . -runTests -testPlatform EditMode -testResults results.xml
/// </summary>
public class SdkInUnityTests
{
    [Test]
    public void EventHelpersWorkOnUnitysNewtonsoft()
    {
        var ev = JObject.Parse("{\"id\":\"sevt_1\",\"type\":\"agent.message\",\"content\":[{\"type\":\"text\",\"text\":\"391\"}]}");
        Assert.AreEqual("agent.message", Events.Type(ev));
        Assert.AreEqual("391", Events.Text(ev));
        var idle = JObject.Parse("{\"type\":\"session.status_idle\",\"stop_reason\":{\"type\":\"requires_action\"}}");
        Assert.IsTrue(Events.RequiresAction(idle));
    }

    [Test]
    public void ClientRejectsANonKey()
    {
        Assert.Throws<ArgumentException>(() => new OpusClient("https://api.opustower.dev/v1", "sk-ant-nope"));
    }

    [UnityTest]
    public IEnumerator LiveMeAndRig_WhenKeyIsSet()
    {
        var key = Environment.GetEnvironmentVariable("OPUS_API_KEY");
        if (string.IsNullOrEmpty(key)) { Assert.Ignore("OPUS_API_KEY not set"); yield break; }
        var task = Run(key);
        while (!task.IsCompleted) yield return null;
        if (task.Exception != null) throw task.Exception.InnerException ?? task.Exception;
    }

    private static async Task Run(string key)
    {
        using var api = new OpusClient("https://api.opustower.dev/v1", key);
        var me = await api.MeAsync();
        Assert.IsNotEmpty(me.KeyId);
        var rig = await api.RigAsync();
        Assert.IsTrue(rig.Configured);
    }
}
