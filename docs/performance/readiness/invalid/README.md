# Invalid comparison

`multi-bytes-varied-stale-artifact.json` must not be interpreted as the byte-event implementation. Both isolated projects were copied/modified before the first build, and Cargo reused the first project's executable from the shared target directory on the second build. Binary SHA-256 checks exposed the identical artifacts despite different source. It actually repeats the multi-thread readiness implementation.

The failed comparison is retained. The byte-event project is rebuilt after refreshing isolated source timestamps, and the corrected executable must have a distinct SHA-256 before retesting. No application-runtime conclusion is drawn from this mislabeled observation.
