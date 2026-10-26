# The only execution platform: whatever `[kbf_gamma]` of `.buckconfig.local` says, which the
# verb renders as remote on, local off, and the properties the organisation sets for the
# farm's Linux workers (`FARM_LINUX_X86_64_PROPERTIES`, `key=value` separated by commas).

def _bool(key):
    value = read_root_config("kbf_gamma", key, "")
    if value not in ("true", "false"):
        fail("kbf_gamma." + key + " must be true or false, got " + repr(value))
    return value == "true"

def _properties():
    text = read_root_config("kbf_gamma", "properties", "")
    if not text:
        fail("kbf_gamma.properties is not set: the verb writes it to .buckconfig.local")
    return dict([entry.split("=", 1) for entry in text.split(",")])

def _remote_platform_impl(ctx):
    properties = ctx.attrs.properties
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = ConfigurationInfo(constraints = {}, values = {}),
        executor_config = CommandExecutorConfig(
            local_enabled = ctx.attrs.local_enabled,
            remote_enabled = ctx.attrs.remote_enabled,
            use_limited_hybrid = False,
            remote_execution_properties = properties,
            remote_execution_use_case = "buck2-default",
            remote_output_paths = "output_paths",
        ),
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

_remote_platform = rule(
    impl = _remote_platform_impl,
    attrs = {
        "local_enabled": attrs.bool(),
        "properties": attrs.dict(key = attrs.string(), value = attrs.string()),
        "remote_enabled": attrs.bool(),
    },
)

def remote_platform(name):
    _remote_platform(
        name = name,
        local_enabled = _bool("local_enabled"),
        remote_enabled = _bool("remote_enabled"),
        properties = _properties(),
    )
