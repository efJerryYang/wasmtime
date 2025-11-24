(module
  (import "env" "log0" (func $log0 (param i32)))
  (import "env" "log1" (func $log1 (param i32)))
  (import "env" "log2" (func $log2 (param i32)))
  (import "env" "sleep_ms" (func $sleep_ms (param i32)))

  (memory (export "memory") 1)

  (func (export "thread0")
    (local $i i32)
    (loop $loop
      local.get $i
      call $log0

      local.get $i
      i32.const 1
      i32.add
      local.set $i

      ;; slow down a bit so interleaving is easy to see
      i32.const 50
      call $sleep_ms

      br $loop
    )
  )

  (func (export "thread1")
    (local $i i32)
    (loop $loop
      local.get $i
      call $log1
      local.get $i
      i32.const 1
      i32.add
      local.set $i
      i32.const 50
      call $sleep_ms
      br $loop
    )
  )

  (func (export "thread2")
    (local $i i32)
    (loop $loop
      local.get $i
      call $log2
      local.get $i
      i32.const 1
      i32.add
      local.set $i
      i32.const 50
      call $sleep_ms
      br $loop
    )
  )
)
