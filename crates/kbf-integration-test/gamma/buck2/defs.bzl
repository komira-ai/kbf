# `step`: one `/bin/sh -c` action. The script runs as `sh -c SCRIPT sh OUT NONCE SRC...`, so
# `$1` is the output file, `$2` the run's nonce and `$3...` the outputs of the steps it
# depends on. The nonce is an argument of every action, so every action is new in each run
# and the farm cannot answer it from its action cache.

def _step_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".txt")
    ctx.actions.run(
        cmd_args("/bin/sh", "-c", ctx.attrs.script, "sh", out.as_output(), ctx.attrs.nonce, ctx.attrs.srcs),
        category = "gamma_step",
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = out)]

_step = rule(
    impl = _step_impl,
    attrs = {
        "nonce": attrs.string(),
        "script": attrs.string(),
        "srcs": attrs.list(attrs.source(), default = []),
    },
)

def step(**kwargs):
    nonce = read_config("kbf_gamma", "nonce", "")
    if not nonce:
        fail("kbf_gamma.nonce is not set: the verb writes it to .buckconfig.local")
    _step(nonce = nonce, **kwargs)
