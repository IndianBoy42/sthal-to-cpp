# STHAL to C++

A little wrapper generator for stm32cube hal and ll. All it does is allow you
to use method call syntax and namespaced functions.

```sh
cargo run --release -- $SRC/ $SRC/Drivers/STM32H7xx_HAL_Driver/ $SRC/Cpp/
```
