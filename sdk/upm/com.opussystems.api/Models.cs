using System.Collections.Generic;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace OpusSystems.Api
{
    // Everything the API composes itself is typed here. Session and event
    // objects are Anthropic's and are exposed as JObject: their shape is
    // Anthropic's contract, and a headset mostly wants event["type"] and
    // the text blocks, which the helpers below extract.

    public sealed class Me
    {
        [JsonProperty("key_id")] public string KeyId { get; set; } = "";
        [JsonProperty("name")] public string Name { get; set; } = "";
        [JsonProperty("scopes")] public List<string> Scopes { get; set; } = new List<string>();
        [JsonProperty("created_at")] public string CreatedAt { get; set; } = "";
    }

    public sealed class Agent
    {
        [JsonProperty("slug")] public string Slug { get; set; } = "";
        [JsonProperty("agent_id")] public string AgentId { get; set; } = "";
        [JsonProperty("agent_version")] public int AgentVersion { get; set; }
        /// <summary>Per-session hard cap, whole US cents as a string ("500" = $5.00).</summary>
        [JsonProperty("max_list_cost_cents")] public string MaxListCostCents { get; set; } = "";
        [JsonProperty("effort")] public string Effort { get; set; } = "";
        [JsonProperty("default_environment")] public string DefaultEnvironment { get; set; } = "";
        [JsonProperty("synced_at")] public string SyncedAt { get; set; } = "";
    }

    public sealed class Rig
    {
        [JsonProperty("configured")] public bool Configured { get; set; }
        [JsonProperty("online")] public bool Online { get; set; }
        [JsonProperty("models")] public List<string> Models { get; set; } = new List<string>();
        [JsonProperty("reason")] public string? Reason { get; set; }
    }

    public sealed class CreateSessionRequest
    {
        [JsonProperty("agent_slug")] public string AgentSlug { get; set; } = "";
        [JsonProperty("task")] public string Task { get; set; } = "";
        [JsonProperty("environment", NullValueHandling = NullValueHandling.Ignore)] public string? Environment { get; set; }
        [JsonProperty("repositories", NullValueHandling = NullValueHandling.Ignore)] public List<string>? Repositories { get; set; }
        /// <summary>Tools this client executes, for this session only. When the agent calls one,
        /// answer the agent.custom_tool_use event with <see cref="OpusClient.SendToolResultsAsync"/>
        /// or <see cref="SessionSocket.SendToolResultAsync"/>.</summary>
        [JsonProperty("tools", NullValueHandling = NullValueHandling.Ignore)] public List<CustomTool>? Tools { get; set; }
        /// <summary>Appended to the agent's system prompt for this session only (persona, device context). ≤ 4000 chars.</summary>
        [JsonProperty("system_suffix", NullValueHandling = NullValueHandling.Ignore)] public string? SystemSuffix { get; set; }
        /// <summary>Which client started it (`quest`, `mac`, `tauri`) — kept as session metadata `iron_fleet_client` so another device can find the session.</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public string? Client { get; set; }
    }

    /// <summary>A client-executed tool. Name is [a-z0-9_]{1,64}; InputSchema is a JSON Schema object.</summary>
    public sealed class CustomTool
    {
        [JsonProperty("type")] public string Type { get; set; } = "custom";
        [JsonProperty("name")] public string Name { get; set; } = "";
        [JsonProperty("description")] public string Description { get; set; } = "";
        [JsonProperty("input_schema")] public JObject InputSchema { get; set; } = new JObject { ["type"] = "object", ["properties"] = new JObject() };
    }

    public sealed class ToolResult
    {
        [JsonProperty("custom_tool_use_id")] public string CustomToolUseId { get; set; } = "";
        [JsonProperty("content")] public string Content { get; set; } = "";
        [JsonProperty("is_error", DefaultValueHandling = DefaultValueHandling.Ignore)] public bool IsError { get; set; }
    }

    public sealed class CreatedSession
    {
        [JsonProperty("session_id")] public string SessionId { get; set; } = "";
        [JsonProperty("status")] public string Status { get; set; } = "";
        [JsonProperty("agent_slug")] public string AgentSlug { get; set; } = "";
        [JsonProperty("agent_id")] public string AgentId { get; set; } = "";
        [JsonProperty("agent_version")] public int AgentVersion { get; set; }
        [JsonProperty("environment")] public string Environment { get; set; } = "";
        [JsonProperty("environment_id")] public string EnvironmentId { get; set; } = "";
        [JsonProperty("budget")] public Budget Budget { get; set; } = new Budget();
        [JsonProperty("console_url")] public string ConsoleUrl { get; set; } = "";
    }

    public sealed class Budget
    {
        [JsonProperty("max_list_cost_cents")] public string MaxListCostCents { get; set; } = "";
    }

    /// <summary>Anthropic's list envelope.</summary>
    public sealed class Page<T>
    {
        [JsonProperty("data")] public List<T> Data { get; set; } = new List<T>();
        [JsonProperty("next_page")] public string? NextPage { get; set; }
        [JsonProperty("prev_page")] public string? PrevPage { get; set; }
    }

    public sealed class AgentUsage
    {
        [JsonProperty("agent_slug")] public string AgentSlug { get; set; } = "";
        [JsonProperty("session_count")] public long SessionCount { get; set; }
        [JsonProperty("total_list_cost_cents")] public long TotalListCostCents { get; set; }
        [JsonProperty("budget_reached_count")] public long BudgetReachedCount { get; set; }
    }

    public sealed class SessionUsage
    {
        [JsonProperty("session_id")] public string SessionId { get; set; } = "";
        [JsonProperty("agent_slug")] public string AgentSlug { get; set; } = "";
        [JsonProperty("environment_slug")] public string? EnvironmentSlug { get; set; }
        [JsonProperty("list_cost_cents")] public string? ListCostCents { get; set; }
        [JsonProperty("input_tokens")] public long? InputTokens { get; set; }
        [JsonProperty("output_tokens")] public long? OutputTokens { get; set; }
        [JsonProperty("active_seconds")] public double? ActiveSeconds { get; set; }
        [JsonProperty("budget_reached")] public bool BudgetReached { get; set; }
        [JsonProperty("last_event_type")] public string LastEventType { get; set; } = "";
        [JsonProperty("observed_at")] public string ObservedAt { get; set; } = "";
        /// <summary>"type: message" when the last turn died on a session.error.</summary>
        [JsonProperty("last_error")] public string? LastError { get; set; }
    }

    public sealed class UsageWindow
    {
        [JsonProperty("since")] public string? Since { get; set; }
        [JsonProperty("until")] public string? Until { get; set; }
    }

    public sealed class Usage
    {
        [JsonProperty("window")] public UsageWindow Window { get; set; } = new UsageWindow();
        [JsonProperty("by_agent")] public List<AgentUsage> ByAgent { get; set; } = new List<AgentUsage>();
        [JsonProperty("recent")] public List<SessionUsage> Recent { get; set; } = new List<SessionUsage>();
    }

    public sealed class VoiceInfo
    {
        [JsonProperty("configured")] public bool Configured { get; set; }
        [JsonProperty("voice_id")] public string VoiceId { get; set; } = "";
        [JsonProperty("model")] public string? Model { get; set; }
    }

    /// <summary>Helpers over Anthropic's event objects.</summary>
    public static class Events
    {
        public static string Type(JObject ev) => ev.Value<string>("type") ?? "";
        public static string Id(JObject ev) => ev.Value<string>("id") ?? "";

        /// <summary>The concatenated text blocks of an agent.message / user.message; "" otherwise.</summary>
        public static string Text(JObject ev)
        {
            if (!(ev["content"] is JArray blocks)) return "";
            var sb = new System.Text.StringBuilder();
            foreach (var b in blocks)
            {
                var t = b.Value<string>("text");
                if (t != null) sb.Append(t);
            }
            return sb.ToString();
        }

        /// <summary>stop_reason.type of a session.status_idle, e.g. "end_turn"; null otherwise.</summary>
        public static string? StopReason(JObject ev) => ev["stop_reason"]?.Value<string>("type");

        /// <summary>For agent.custom_tool_use: the tool name; null otherwise.</summary>
        public static string? ToolName(JObject ev) => Type(ev) == "agent.custom_tool_use" ? ev.Value<string>("name") : null;

        /// <summary>For agent.custom_tool_use: the input object (may be empty).</summary>
        public static JObject ToolInput(JObject ev) => ev["input"] as JObject ?? new JObject();

        /// <summary>True when a session.status_idle is the wait for a tool result, not the end of the turn.</summary>
        public static bool RequiresAction(JObject ev) => StopReason(ev) == "requires_action";

        /// <summary>"type: message" of a session.error; null otherwise.</summary>
        public static string? Error(JObject ev)
        {
            var e = ev["error"];
            if (e == null) return null;
            return $"{e.Value<string>("type")}: {e.Value<string>("message")}";
        }
    }
}
