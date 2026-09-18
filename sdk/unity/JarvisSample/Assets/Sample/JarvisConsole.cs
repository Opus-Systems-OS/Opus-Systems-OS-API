using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Text;
using System.Threading.Tasks;
using OpusSystems.Api;
using UnityEngine;

/// <summary>
/// The smallest possible fleet client in Unity: a key, a text box, a Send
/// button, and the reply streaming in over the session WebSocket. Runs in
/// Play mode in the editor with no headset; the Quest app grows from here
/// (swap the IMGUI for a world-space panel and the text box for speech).
///
/// Everything from the SDK arrives on background threads; this component
/// queues it and applies it in Update, which is the one rule for using the
/// SDK in Unity.
/// </summary>
public sealed class JarvisConsole : MonoBehaviour
{
    [Tooltip("Base URL including /v1.")]
    public string baseUrl = "https://api.opustower.dev/v1";

    [Tooltip("osk_… key with sessions:read and sessions:write. Leave empty to use the OPUS_API_KEY environment variable or the saved one.")]
    public string apiKey = "";

    private OpusClient _api;
    private SessionSocket _ws;
    private string _sessionId = "";
    private string _draft = "What's seventeen times twenty-three?";
    private readonly StringBuilder _transcript = new StringBuilder();
    private readonly StringBuilder _partial = new StringBuilder();
    private string _status = "idle";
    private bool _busy;
    private Vector2 _scroll;

    // Cross-thread: SDK callbacks enqueue, Update drains.
    private readonly ConcurrentQueue<Action> _mainThread = new ConcurrentQueue<Action>();

    private const string KeyPref = "opus.api.key";

    private void Start()
    {
        if (string.IsNullOrEmpty(apiKey))
            apiKey = Environment.GetEnvironmentVariable("OPUS_API_KEY") ?? PlayerPrefs.GetString(KeyPref, "");
    }

    private void Update()
    {
        while (_mainThread.TryDequeue(out var action)) action();
    }

    private void OnDestroy()
    {
        _ws?.Dispose();
        _api?.Dispose();
    }

    // ---- the loop ---------------------------------------------------------

    private async void Send()
    {
        var text = _draft.Trim();
        if (text.Length == 0 || _busy) return;
        if (!apiKey.StartsWith("osk_"))
        {
            _status = "add an osk_ key first";
            return;
        }
        _busy = true;
        _draft = "";
        _transcript.Append("You: ").Append(text).Append('\n');
        _partial.Clear();
        _status = "thinking…";

        try
        {
            _api ??= new OpusClient(baseUrl, apiKey);
            PlayerPrefs.SetString(KeyPref, apiKey);

            if (_ws == null)
            {
                // First message: create the session (this client's persona
                // as a session-local suffix; tools would go here too), then
                // open the socket with deltas so the reply streams.
                var created = await _api.CreateSessionAsync(new CreateSessionRequest
                {
                    AgentSlug = "jarvis",
                    Task = text,
                    SystemSuffix = "You are speaking through a Unity app on a headset prototype. Plain sentences, no markdown, one to three sentences.",
                });
                _sessionId = created.SessionId;
                _ws = await _api.OpenSessionSocketAsync(_sessionId, history: false, deltas: true);
                _ws.OnDelta += (_, fragment) => _mainThread.Enqueue(() => _partial.Append(fragment));
                _ws.OnEvent += ev => _mainThread.Enqueue(() => OnEvent(ev));
                _ws.OnError += (type, message) => _mainThread.Enqueue(() => { _status = $"error {type}: {message}"; _busy = false; });
                _ws.OnClosed += reason => _mainThread.Enqueue(() => { _status = $"closed ({reason})"; _ws = null; _busy = false; });
            }
            else
            {
                await _ws.SendAsync(text);
            }
        }
        catch (OpusApiException e)
        {
            _status = $"{(int)e.Status} {e.Type}: {e.Message}";
            _busy = false;
        }
        catch (Exception e)
        {
            _status = e.Message;
            _busy = false;
        }
    }

    private void OnEvent(Newtonsoft.Json.Linq.JObject ev)
    {
        switch (Events.Type(ev))
        {
            case "agent.message":
                // The authoritative text replaces the streamed preview.
                _transcript.Append("Jarvis: ").Append(Events.Text(ev)).Append('\n');
                _partial.Clear();
                break;
            case "agent.custom_tool_use":
                // This sample declares no tools; a real client runs the tool
                // here and answers with _ws.SendToolResultAsync(id, result).
                _status = $"tool requested: {Events.ToolName(ev)}";
                break;
            case "session.status_idle":
                if (Events.RequiresAction(ev)) break; // waiting on a tool result
                _status = Events.StopReason(ev) == "end_turn" ? "idle" : $"idle ({Events.StopReason(ev)})";
                _busy = false;
                break;
            case "session.error":
                _status = Events.Error(ev) ?? "session error";
                _busy = false;
                break;
        }
    }

    // ---- IMGUI, so the sample needs no scene setup ---------------------------

    private void OnGUI()
    {
        const int pad = 12;
        var w = Screen.width - 2 * pad;
        GUILayout.BeginArea(new Rect(pad, pad, w, Screen.height - 2 * pad));

        GUILayout.Label($"Opus Systems OS — Jarvis   [{_status}]   {(string.IsNullOrEmpty(_sessionId) ? "" : _sessionId)}");
        GUILayout.BeginHorizontal();
        GUILayout.Label("Key", GUILayout.Width(30));
        apiKey = GUILayout.PasswordField(apiKey, '•');
        GUILayout.EndHorizontal();

        _scroll = GUILayout.BeginScrollView(_scroll, GUILayout.ExpandHeight(true));
        var shown = _transcript.ToString();
        if (_partial.Length > 0) shown += "Jarvis: " + _partial + " ▍";
        GUILayout.TextArea(shown, GUILayout.ExpandHeight(true));
        GUILayout.EndScrollView();

        GUILayout.BeginHorizontal();
        _draft = GUILayout.TextField(_draft);
        GUI.enabled = !_busy;
        if (GUILayout.Button("Send", GUILayout.Width(80))) Send();
        GUI.enabled = true;
        if (GUILayout.Button("New", GUILayout.Width(60)))
        {
            _ws?.Dispose();
            _ws = null;
            _sessionId = "";
            _transcript.Clear();
            _partial.Clear();
            _status = "idle";
            _busy = false;
        }
        GUILayout.EndHorizontal();
        GUILayout.EndArea();
    }
}
