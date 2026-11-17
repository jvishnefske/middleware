# checks.robot-pi-boots (design.md MS50-MS52, MS54): nix/robot-pi.nix, the
# Pi image's role without the Pi hardware, boots in an x86_64 VM with 512 MiB
# of RAM and no Pico attached.
#
#   * Envoy and the robot app come up, spire-agent and sshd are held back by
#     their path conditions, mw-wifi never starts (no wlan0), and no unit
#     fails
#   * mw-gateway stays inactive (not failed) and is bound to the
#     dev-mw\x2dpico.device unit; its restart policy backs off and never
#     hits a start limit; it is the only service user in dialout
#   * 70-mw-pico.rules is installed and `udevadm verify` accepts it
#   * zram swap is active, and the memory every unit of ours uses is logged
#   * a FAT disk labelled FIRMWARE with mw-robot/{join-token,robot.env,
#     authorized_keys} is copied into /var/lib/mw-robot (0600, root) by
#     mw-provision and emptied
#   * with authorized_keys present sshd starts and refuses passwords
#   * robot.env's MW_NET_ADDRESS gives eth0 that static address through
#     /run/systemd/network; a malformed one leaves DHCP alone
{ pkgs, piModule }:
pkgs.testers.runNixOSTest {
  name = "robot-pi-boots";

  nodes.pi = { lib, ... }: {
    imports = [ piModule ];
    # The test driver sets its own root password file.
    users.users.root.hashedPassword = lib.mkForce null;
    virtualisation.memorySize = 512;
    virtualisation.emptyDiskImages = [ 16 ];
    environment.systemPackages = [ pkgs.curl pkgs.dosfstools pkgs.mtools ];
  };

  testScript = ''
    pi.start()
    pi.wait_for_unit("multi-user.target")

    with subtest("services up without a Pico, nothing failed"):
        pi.wait_for_unit("mw-envoy.service")
        pi.wait_until_succeeds("curl -sf http://127.0.0.1:15000/ready")
        pi.wait_until_succeeds("journalctl -u mw-robot.service --grep 'Started' -q")
        t.assertIn("ConditionResult=no", pi.succeed("systemctl show -p ConditionResult spire-agent.service"))
        t.assertIn("ConditionResult=no", pi.succeed("systemctl show -p ConditionResult sshd.service"))
        t.assertIn("ActiveState=inactive", pi.succeed("systemctl show -p ActiveState mw-wifi.service"))
        pi.succeed("systemctl is-active mw-provision.service mw-net-config.service")
        failed = pi.succeed("systemctl list-units --failed --no-legend --plain").strip()
        t.assertEqual("", failed)

    with subtest("gateway waits for the board"):
        show = pi.succeed("systemctl show -p ActiveState,Result,BindsTo,User,SupplementaryGroups,StartLimitIntervalUSec,RestartUSec,RestartSteps,RestartMaxDelayUSec,ExecStart mw-gateway.service")
        print(show)
        t.assertIn("ActiveState=inactive", show)
        t.assertIn("Result=success", show)
        # systemctl show quotes and escapes the backslash of \x2d.
        t.assertIn("x2dpico.device", show.split("BindsTo=")[1].splitlines()[0])
        t.assertIn("User=mw-gateway", show)
        t.assertIn("SupplementaryGroups=dialout", show)
        t.assertIn("StartLimitIntervalUSec=0", show)
        t.assertIn("RestartSteps=6", show)
        t.assertIn("RestartMaxDelayUSec=1min", show)
        t.assertIn("MW_GATEWAY_SERIAL=/dev/mw-pico", show)
        pi.fail("test -e /dev/mw-pico")
        t.assertIn("dialout", pi.succeed("id -nG mw-gateway"))
        for user in ["robot", "mw-envoy"]:
            t.assertNotIn("dialout", pi.succeed(f"id -nG {user}"))

    with subtest("udev rule installed and valid"):
        rules = pi.succeed("cat /etc/udev/rules.d/70-mw-pico.rules")
        for word in ['ATTRS{idVendor}=="2e8a"', 'ATTRS{idProduct}=="000a"', 'ATTRS{idProduct}=="0009"',
                     'ATTRS{manufacturer}=="middleware"', 'ATTRS{product}=="rp-base"', 'SYMLINK+="mw-pico"']:
            t.assertIn(word, rules)
        pi.succeed("udevadm verify --no-style /etc/udev/rules.d/70-mw-pico.rules")

    with subtest("memory: zram swap and what our units use"):
        t.assertIn("/dev/zram0", pi.succeed("swapon --show=NAME --noheadings"))
        print(pi.succeed("free -m"))
        for unit in ["mw-envoy", "mw-robot", "mw-provision", "systemd-journald"]:
            print(pi.succeed(f"systemctl show -p MemoryCurrent,MemoryMax {unit}.service"))

    with subtest("provision from the boot partition"):
        pi.succeed("mkfs.vfat -n FIRMWARE /dev/vdb")
        pi.succeed("udevadm settle && test -e /dev/disk/by-label/FIRMWARE")
        pi.succeed("mmd -i /dev/vdb ::mw-robot")
        pi.succeed("echo token-from-fat > /tmp/join-token && mcopy -i /dev/vdb /tmp/join-token ::mw-robot/join-token")
        pi.succeed("echo MW_ROBOT_NAME=pi-test > /tmp/robot.env && mcopy -i /dev/vdb /tmp/robot.env ::mw-robot/robot.env")
        pi.succeed("ssh-keygen -q -t ed25519 -N ''' -f /root/key")
        pi.succeed("mcopy -i /dev/vdb /root/key.pub ::mw-robot/authorized_keys")
        pi.succeed("systemctl restart mw-provision.service")
        t.assertEqual("token-from-fat", pi.succeed("cat /var/lib/mw-robot/join-token").strip())
        for f in ["join-token", "robot.env", "authorized_keys"]:
            t.assertEqual("600 root", pi.succeed(f"stat -c '%a %U' /var/lib/mw-robot/{f}").strip())
        t.assertEqual("", pi.succeed("mdir -b -i /dev/vdb ::mw-robot").strip())

    with subtest("sshd with a key file: up, key only"):
        pi.succeed("systemctl start sshd.service")
        pi.wait_for_unit("sshd.service")
        conf = pi.succeed("sshd -T").lower()
        t.assertIn("passwordauthentication no", conf)
        t.assertIn("kbdinteractiveauthentication no", conf)
        t.assertIn("authorizedkeysfile /var/lib/mw-robot/authorized_keys", conf)
        pi.wait_for_open_port(22)
        pi.succeed("ssh -i /root/key -o StrictHostKeyChecking=no -o BatchMode=yes root@127.0.0.1 true")

    with subtest("static address from robot.env"):
        pi.succeed("printf 'MW_NET_ADDRESS=bogus\\n' > /var/lib/mw-robot/robot.env")
        pi.succeed("systemctl restart mw-net-config.service")
        pi.fail("test -e /run/systemd/network/40-mw-static.network")
        pi.succeed(
            "printf '%s\\n' MW_NET_ADDRESS=10.66.0.21/24 MW_NET_INTERFACE=eth0 "
            "MW_NET_GATEWAY=10.66.0.1 'MW_NET_DNS=10.66.0.1 9.9.9.9' > /var/lib/mw-robot/robot.env"
        )
        pi.succeed("systemctl restart mw-net-config.service")
        net = pi.succeed("cat /run/systemd/network/40-mw-static.network")
        print(net)
        for line in ["Name=eth0", "Address=10.66.0.21/24", "Gateway=10.66.0.1", "DNS=10.66.0.1", "DNS=9.9.9.9"]:
            t.assertIn(line, net)
        pi.succeed("networkctl reload && networkctl reconfigure eth0")
        pi.wait_until_succeeds("ip -4 addr show eth0 | grep -q 10.66.0.21/24")
  '';
}
