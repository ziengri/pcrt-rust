# Inference fixtures

These fixtures define the Python reference output before the native Rust
`InferenceBackend` is enabled. A fixture names the exact video, OpenVINO model,
processing parameters, expected counts and SHA-256 digests of every input.

The expected counts are produced by the current Python `SessionAIRunner`
pipeline, which uses OpenCV decode, `OpenVINODetector`, Norfair and
`PeopleCounter`. They are a compatibility baseline, not ground-truth labels.

Regenerate a fixture only after an intentional Python algorithm or model change:

```text
uv run python scripts/record_inference_baseline.py \
  --fixture rust/contracts/inference/fixtures/v1/4-mp4-yolo26n-head-v3.json
```

The Rust adapter is not production-ready until it produces the same counts for
each fixture and a shadow run confirms behavior on recorded device sessions.
