# Benchmark baselines

JSON files record reproducible local performance baselines. They are not universal SLA values: filesystem, CPU cache, kernel, and root shape affect results.

Regenerate event measurements with:

```bash
./scripts/benchmark_event_updates.py --iterations 5
./scripts/integration_test.py
```

For a performance change, record the command, root shape, sample count, and before/after median, p95, and maximum before updating a baseline.
