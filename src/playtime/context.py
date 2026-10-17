"""Conservative context accounting and explicit limits on new tool results."""

import copy

from .common import canonical

TEMPLATE_RESERVE = 4096
CONTROL_RESERVE = 4096


def encoded_size(value):
    return len(canonical(value).encode())


class ContextWindow:
    def __init__(self):
        self.messages = []
        self.tools = []
        self.tokens = None

    def observe(self, messages, tools, response):
        prompt = response.get("prompt_eval_count")
        if type(prompt) is not int or prompt <= 0:
            return
        self.messages = copy.deepcopy([*messages, response["message"]])
        self.tools = tools
        # The generated count includes reasoning, even when the server omits it
        # from the next prompt. Template/serialization differences have a separate
        # reserve. Never use the preceding session's count for a fresh context.
        self.tokens = prompt + response["eval_count"]

    def estimate(self, messages, tools):
        count = len(self.messages)
        if self.tokens is not None and messages[:count] == self.messages:
            extra = encoded_size(messages[count:])
            if tools != self.tools:
                extra += encoded_size(tools)
            return self.tokens + extra, "server_usage_plus_new_bytes"
        return encoded_size({"messages": messages, "tools": tools}), "input_bytes"


def tool_message(name, result):
    return {"role": "tool", "tool_name": name or "unknown", "content": canonical(result)}


def fit_tool_result(name, result, maximum):
    """Shorten only this new result; never remove earlier conversation text.

    The caller records the full bounded tool result as well as this exact
    delivered result. Measure the JSON message, including escaped output.
    """
    if encoded_size(tool_message(name, result)) <= maximum:
        return result
    limited = {
        key: result[key]
        for key in ("exit_code", "timed_out", "bytes_written", "offset_bytes", "size_bytes")
        if key in result
    }
    limited.update(
        context_truncated=True,
        notice="Tool output shortened to reserve space for the session handoff.",
    )
    # Retain error/success metadata and as much of the text as fits. Both JSON
    # encodings can expand quotes, newlines, and control characters.
    fields = [key for key in ("error", "stderr", "stdout", "content") if key in result]
    for key in fields:
        low, high = 0, len(result[key])
        while low < high:
            middle = (low + high + 1) // 2
            candidate = {**limited, key: result[key][:middle]}
            if encoded_size(tool_message(name, candidate)) <= maximum:
                low = middle
            else:
                high = middle - 1
        if low:
            limited[key] = result[key][:low]
    return limited
