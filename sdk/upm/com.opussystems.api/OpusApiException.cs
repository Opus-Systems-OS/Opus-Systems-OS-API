using System;
using System.Net;

namespace OpusSystems.Api
{
    /// <summary>
    /// Every non-2xx from the API. Mirrors the envelope
    /// <c>{"error":{"type","message","request_id"}}</c>; quote
    /// <see cref="RequestId"/> when reporting a problem.
    /// </summary>
    public sealed class OpusApiException : Exception
    {
        public HttpStatusCode Status { get; }
        /// <summary>unauthorized, forbidden, not_found, conflict, invalid_request,
        /// method_not_allowed, rate_limited, upstream, rig_offline, internal.</summary>
        public string Type { get; }
        public string RequestId { get; }
        /// <summary>Seconds to wait, when the API said so (429, 503 rig_offline).</summary>
        public int? RetryAfterSeconds { get; }

        public OpusApiException(HttpStatusCode status, string type, string message, string requestId, int? retryAfterSeconds)
            : base($"{(int)status} {type}: {message} (request {requestId})")
        {
            Status = status;
            Type = type;
            RequestId = requestId;
            RetryAfterSeconds = retryAfterSeconds;
        }

        public bool IsRigOffline => Type == "rig_offline";
        public bool IsRateLimited => Type == "rate_limited";
    }
}
