// Behavioural ECP5 DP16KD for simulation, only the configuration that
// wasm.ecp5_tdp_bytes uses: both ports 16384 x 1 (DATA_WIDTH 1, address on
// AD[13:0], data on DI0/DO0), REGMODE NOREG (the read data is registered at
// the clock edge), WRITEMODE WRITETHROUGH (a write shows the new bit on the
// port's output), chip selects and resets tied off. Yosys's own DP16KD in
// cells_sim_ecp5.v is a blackbox, so it cannot stand in for this.
module DP16KD #(
    parameter DATA_WIDTH_A = 18, parameter DATA_WIDTH_B = 18,
    parameter REGMODE_A = "NOREG", parameter REGMODE_B = "NOREG",
    parameter RESETMODE = "SYNC",
    parameter CSDECODE_A = "0b000", parameter CSDECODE_B = "0b000",
    parameter WRITEMODE_A = "NORMAL", parameter WRITEMODE_B = "NORMAL",
    parameter GSR = "ENABLED"
) (
    input DIA0, ADA13, ADA12, ADA11, ADA10, ADA9, ADA8, ADA7, ADA6, ADA5, ADA4, ADA3, ADA2,
    input ADA1, ADA0, CEA, OCEA, CLKA, WEA, RSTA, CSA2, CSA1, CSA0,
    output reg DOA0,
    input DIB0, ADB13, ADB12, ADB11, ADB10, ADB9, ADB8, ADB7, ADB6, ADB5, ADB4, ADB3, ADB2,
    input ADB1, ADB0, CEB, OCEB, CLKB, WEB, RSTB, CSB2, CSB1, CSB0,
    output reg DOB0
);
    initial begin
        if (DATA_WIDTH_A != 1 || DATA_WIDTH_B != 1 || REGMODE_A != "NOREG" || REGMODE_B != "NOREG"
                || WRITEMODE_A != "WRITETHROUGH" || WRITEMODE_B != "WRITETHROUGH")
            $fatal(1, "dp16kd_x1.v models only x1, NOREG, WRITETHROUGH");
    end
    reg mem [0:16383];
    wire [13:0] ada = {ADA13, ADA12, ADA11, ADA10, ADA9, ADA8, ADA7, ADA6, ADA5, ADA4, ADA3,
                       ADA2, ADA1, ADA0};
    wire [13:0] adb = {ADB13, ADB12, ADB11, ADB10, ADB9, ADB8, ADB7, ADB6, ADB5, ADB4, ADB3,
                       ADB2, ADB1, ADB0};
    always @(posedge CLKA) if (CEA) begin
        if (WEA) begin mem[ada] <= DIA0; DOA0 <= DIA0; end
        else DOA0 <= mem[ada];
    end
    always @(posedge CLKB) if (CEB) begin
        if (WEB) begin mem[adb] <= DIB0; DOB0 <= DIB0; end
        else DOB0 <= mem[adb];
    end
endmodule
