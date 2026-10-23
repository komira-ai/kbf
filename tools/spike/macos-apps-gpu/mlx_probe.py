import sys, time
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
    try:
        print("MLX device_info", mx.metal.device_info())
    except Exception as e:
        print("MLX device_info FAIL", repr(e))
    print("MLX_PROBE_PASS")
except Exception as e:
    print("MLX_PROBE_FAIL", repr(e)); sys.exit(3)
