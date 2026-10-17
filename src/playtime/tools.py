TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "exec",
            "description": "Execute a Bash command in the workshop. Save long output to files. Background processes stop when this session ends.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_seconds": {"type": "number"},
                },
                "required": ["command"],
                "additionalProperties": False,
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a bounded slice of a UTF-8 text file in the workshop.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}, "offset_bytes": {"type": "integer"}},
                "required": ["path"],
                "additionalProperties": False,
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Write or append UTF-8 text to a file inside /work.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "append": {"type": "boolean"},
                },
                "required": ["path", "content"],
                "additionalProperties": False,
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "end_session",
            "description": "Save your memento and end this session. Set finish_run only if the entire exploration is complete.",
            "parameters": {
                "type": "object",
                "properties": {"memento": {"type": "string"}, "finish_run": {"type": "boolean"}},
                "required": ["memento"],
                "additionalProperties": False,
            },
        },
    },
]


def validate_call(name, arguments):
    schema = next(
        (t["function"]["parameters"] for t in TOOLS if t["function"]["name"] == name), None
    )
    if schema is None or not isinstance(arguments, dict):
        return "Unknown tool or invalid arguments object"
    if set(arguments) - set(schema["properties"]) or set(schema["required"]) - set(arguments):
        return "Unexpected or missing tool arguments"
    types = {"string": (str,), "boolean": (bool,), "integer": (int,), "number": (int, float)}
    for key, value in arguments.items():
        if type(value) not in types[schema["properties"][key]["type"]]:
            return f"Invalid type for {key}"
    return None
