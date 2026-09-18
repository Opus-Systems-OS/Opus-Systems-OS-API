# OpusSystems.Api — C# client

The Opus Systems OS API for .NET and Unity: REST (`OpusClient`) and the
session WebSocket (`SessionSocket`). `netstandard2.1`, one dependency
(Newtonsoft.Json — in Unity, `com.unity.nuget.newtonsoft-json`).

Hand-written rather than generated from the spec on purpose: generated C#
clients pull in dependencies Unity chokes on, and the WebSocket — the part
a headset lives on — is not in OpenAPI. `/v1/openapi.json` remains the
contract; the SDK is checked against the live API by `LiveTests`.

```csharp
using OpusSystems.Api;

var api = new OpusClient("https://api.opustower.dev/v1", "osk_…");
var me = await api.MeAsync();                       // who am I, which scopes
var rig = await api.RigAsync();                     // {Configured, Online, Models, Reason}

var s = await api.CreateSessionAsync(new CreateSessionRequest {
    AgentSlug = "jarvis", Task = "What's the weather like on Mars?" });

using var ws = await api.OpenSessionSocketAsync(s.SessionId);
ws.OnEvent += ev => {
    if (Events.Type(ev) == "agent.message") Say(Events.Text(ev));   // marshal to the main thread in Unity
    if (Events.Type(ev) == "session.status_idle") Done();
};
ws.OnError += (type, message) => Debug.LogWarning($"{type}: {message}");
await ws.SendAsync("And on Venus?");
await ws.InterruptAsync();
```

Every non-2xx is an `OpusApiException` with `Type` (`unauthorized`,
`forbidden`, `not_found`, `invalid_request`, `rate_limited`, `rig_offline`,
`upstream`, …), `Status`, `RequestId` and `RetryAfterSeconds`.

## Build and test

```sh
cd sdk/csharp
dotnet build -c Release            # OpusSystems.Api/bin/Release/netstandard2.1/OpusSystems.Api.dll
dotnet test                        # unit tests; live tests skip without a key
OPUS_API_KEY=osk_… dotnet test     # + live: me, agents, rig, usage, error mapping
OPUS_API_KEY=osk_… OPUS_LIVE_SESSION=1 dotnet test   # + a jarvis turn over the WebSocket (a few cents)
```

## Unity

Copy `OpusSystems.Api/*.cs` into `Assets/OpusSystems/` with an assembly
definition referencing `Newtonsoft.Json`, or drop the built DLL into
`Assets/Plugins/`. Handlers on `SessionSocket` fire on a background
thread; hop to the main thread before touching scene objects.
