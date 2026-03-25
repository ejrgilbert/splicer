;; Incompatible middleware for wasi:logging/log@0.1.0.
;;
;; Exports the same interface name but with a DIFFERENT `log` signature
;; (`level` is `string` here vs. `u32` in the chain), so fingerprints will
;; NOT match and validate_contract returns Error.
(component
    (import "wasi:logging/log@0.1.0" (instance $handler
        (export "log" (func (param "level" string) (param "message" string)))
    ))
    (export "wasi:logging/log@0.1.0" (instance $handler))
)
