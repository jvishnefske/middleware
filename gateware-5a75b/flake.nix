{
  # Toolchain for the Colorlight 5A-75B gateware: yosys, nextpnr-ecp5,
  # Project Trellis (ecppack), openFPGALoader, Verilator, and a Python
  # with Migen, LiteX, LiteEth, litex-boards, cocotb and pytest.
  #
  #   nix develop ./gateware-5a75b      # from the repository root
  #
  # LiteX and its cores are not in nixpkgs, so they are built here from
  # pinned release tags.
  description = "middleware gateware for the Colorlight 5A-75B (ECP5)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/0bb2eedc20e4739f29e8045034f2828ad7054f8f";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      py = pkgs.python3.override {
        packageOverrides = pyself: pysuper: {
          litex = pyself.buildPythonPackage {
            pname = "litex";
            version = "2026.08";
            pyproject = true;
            src = pkgs.fetchFromGitHub {
              owner = "enjoy-digital";
              repo = "litex";
              rev = "2026.08";
              hash = "sha256-Z2QK3EvzZ/EypPzrOwhshInDXnDzG329p3+umkZpNX0=";
            };
            build-system = [ pyself.setuptools ];
            dependencies = with pyself; [ migen pyserial requests packaging pyyaml ];
            doCheck = false;
            pythonImportsCheck = [ "litex" ];
          };
          liteiclink = pyself.buildPythonPackage {
            pname = "liteiclink";
            version = "2026.08";
            pyproject = true;
            src = pkgs.fetchFromGitHub {
              owner = "enjoy-digital";
              repo = "liteiclink";
              rev = "2026.08";
              hash = "sha256-HX847jxcFYBHBUJr/PWQJcKGJuVHeq8zBXpBb7B9gYQ=";
            };
            build-system = [ pyself.setuptools ];
            dependencies = [ pyself.litex ];
            doCheck = false;
            pythonImportsCheck = [ "liteiclink" ];
          };
          liteeth = pyself.buildPythonPackage {
            pname = "liteeth";
            version = "2026.08";
            pyproject = true;
            src = pkgs.fetchFromGitHub {
              owner = "enjoy-digital";
              repo = "liteeth";
              rev = "2026.08";
              hash = "sha256-MfAx3DR0ojV3FEKl0mAABCaJt80syimciSWmj3Kyr6g=";
            };
            build-system = [ pyself.setuptools ];
            dependencies = [ pyself.litex pyself.liteiclink ];
            doCheck = false;
            pythonImportsCheck = [ "liteeth" ];
          };
          litex-boards = pyself.buildPythonPackage {
            pname = "litex-boards";
            version = "2026.08";
            pyproject = true;
            src = pkgs.fetchFromGitHub {
              owner = "litex-hub";
              repo = "litex-boards";
              rev = "2026.08";
              hash = "sha256-4sUSahrZnXGJjDZD1NdY6/8cx1gfYMfBcyeqcg6iroQ=";
            };
            build-system = [ pyself.setuptools ];
            dependencies = [ pyself.litex ];
            doCheck = false;
            pythonImportsCheck = [ "litex_boards" ];
          };
        };
      };
      python = py.withPackages (p: with p; [ migen litex liteeth litex-boards cocotb pytest ]);
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          python
          pkgs.yosys
          pkgs.nextpnr
          pkgs.trellis
          pkgs.openfpgaloader
          pkgs.verilator
          pkgs.gnumake
        ];
      };
    };
}
