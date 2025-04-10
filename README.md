
## Connections

## Serial
UART0 (`GP0`/`GP1`) - connected to mux on CH340C on picocalc
UART1 (`GP8`/`GP9`) - connected to `M_UART3` aka `Serial1` on picocalc mcu. Default mcu firmware writes pmu debug logs to this.

## I2C
I2C1 (`GP6`/`GP7`) - I2C bus connected to picocalc keyboard/pmu mcu `M_I2C1`

## LCD
* `GP10` - `SPI1_SCK`
* `GP11` - `SPI1_TX`
* `GP12` - `SPI1_RX`
* `GP13` - `SPI1_CS`
* `GP14` - `LCD_DC`
* `GP15` - `LCD_RST`

## Audio
GP26/GP27 - `PWM_L`/`PWM_R` on picocalc audio circuitry

## TF Card reader
* `GP16` - `SPI0_RX`
* `GP17` - `SPI0_CS`
* `GP18` - `SPI0_SCK`
* `GP19` - `SPI0_TX`
* `GP22` - `SD_DET`

## PSRAM
* `GP2`  - `RAM_TX`
* `GP3`  - `RAM_RX`
* `GP4`  - `RAM_IO2`  - quad mode
* `GP5`  - `RAM_IO3`  - quad mode
* `GP20` - `RAM_CS`
* `GP21` - `RAM_SCK`

Note that all except the CS are exposed to expansion/jumper block.

## Expansion Port/Jumper block
* `GP2`  - Also connected to PSRAM
* `GP3`  - Also connected to PSRAM
* `GP4`  - Also connected to PSRAM
* `GP5`  - Also connected to PSRAM
* `GP21` - Also connected to PSRAM
* `GP28`

