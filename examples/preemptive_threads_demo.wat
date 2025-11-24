(module
  (import "env" "log0" (func $log0 (param i32)))
  (import "env" "log1" (func $log1 (param i32)))

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
      br $loop
    )
  )
)
