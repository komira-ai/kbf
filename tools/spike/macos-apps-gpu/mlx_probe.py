import os, sys, time
try:
    import mlx.core as mx
except Exception as e:
    print("MLX_IMPORT_FAIL", repr(e)); sys.exit(2)
print("MLX version", getattr(mx, "__version__", "?"))
try:
    print("MLX default_device", mx.default_device(), "metal_available", mx.metal.is_available())
    a = mx.random.normal((1024, 1024)); b = mx.random.normal((1024, 1024))
    t = time.time(); c = mx.matmul(a, b, stream=mx.gpu); mx.eval(c)
    print("MLX gpu matmul ms", int((time.time() - t) * 1000))
    d = mx.matmul(a, b, stream=mx.cpu); mx.eval(d)
    print("MLX gpu==cpu", bool(mx.allclose(c, d, atol=1e-2)))
    # mx.compile fuses the function into a generated kernel that is built at run time.
    try:
        f = mx.compile(lambda x, y: mx.exp(x) * mx.tanh(y) + x * y - mx.sin(x))
        t = time.time(); r = f(a, b); mx.eval(r)
        print("MLX compile ms", int((time.time() - t) * 1000))
    except Exception as e:
        print("MLX compile FAIL", repr(e))
    try:
        print("MLX device_info", mx.metal.device_info())
    except Exception as e:
        print("MLX device_info FAIL", repr(e))
    print("MLX_PROBE_PASS")
    if os.environ.get("GPU_HOLD"):
        print("HOLD_READY", flush=True)
        time.sleep(900)
except Exception as e:
    print("MLX_PROBE_FAIL", repr(e)); sys.exit(3)
