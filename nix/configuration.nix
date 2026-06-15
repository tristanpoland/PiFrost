{ config, pkgs, lib, ... }:

{
  # ────────────────────────────────────────────────────────────
  # Minimal system — bare enough to boot and run K3s
  # ────────────────────────────────────────────────────────────
  system.stateVersion = "24.11";
  networking.hostName = "pifrost-node";
  networking.useDHCP = true;
  nixpkgs.hostPlatform = "x86_64-linux";

  services.xserver.enable = false;
  documentation.enable = false;
  documentation.nixos.enable = false;

  # ────────────────────────────────────────────────────────────
  # Boot & Kernel
  # ────────────────────────────────────────────────────────────
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = false;

  boot.kernelParams = [
    "cgroup_enable=cpuset"
    "cgroup_enable=memory"
    "cgroup_memory=1"
  ];

  boot.kernelModules = [ "overlay" "br_netfilter" "nf_conntrack" "nf_tables" ];
  boot.kernel.sysctl = {
    "net.ipv4.ip_forward" = 1;
    "net.ipv6.conf.all.forwarding" = 1;
    "net.bridge.bridge-nf-call-iptables" = 1;
    "net.bridge.bridge-nf-call-ip6tables" = 1;
    "net.ipv4.conf.all.rp_filter" = 1;
    "kernel.panic" = 10;
    "kernel.panic_on_oops" = 1;
    "vm.overcommit_memory" = 1;
  };

  # ────────────────────────────────────────────────────────────
  # Filesystem layout — matches pifrost bootstrap partitioning
  #   p1: system-boot  (512MB FAT32)
  #   p2: writable     (4GB EXT4)  — Nix store lives here
  #   p3: kube-state   (remainder) — persistent K3s data
  # ────────────────────────────────────────────────────────────
  fileSystems."/boot" = {
    device = "/dev/disk/by-label/ESP";
    fsType = "vfat";
  };

  fileSystems."/" = {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
  };

  fileSystems."/mnt/kube-state" = {
    device = "/dev/disk/by-label/kube-state";
    fsType = "ext4";
    options = [ "rw" "noatime" "nodiratime" "errors=remount-ro" ];
    neededForBoot = true;
  };

  # ────────────────────────────────────────────────────────────
  # Impermanence — persist K3s runtime data on kube-state
  # ────────────────────────────────────────────────────────────
  environment.persistence."/mnt/kube-state" = {
    hideMounts = false;
    directories = [
      "/var/lib/nixos"
      "/var/lib/rancher/k3s"
      "/var/lib/containerd"
      "/etc/rancher/k3s"
    ];
    files = [
      "/etc/machine-id"
    ];
  };

  # ────────────────────────────────────────────────────────────
  # Symlinks — runtime data flows to persistent partition
  # ────────────────────────────────────────────────────────────
  systemd.tmpfiles.rules = [
    "L+ /var/lib/rancher/k3s    - - - - /mnt/kube-state/k3s"
    "L+ /var/lib/containerd     - - - - /mnt/kube-state/containerd"
    "L+ /etc/rancher/k3s        - - - - /mnt/kube-state/etc/k3s"
  ];

  # ────────────────────────────────────────────────────────────
  # K3s — installed but NOT auto-started (kube-identity decides)
  # ────────────────────────────────────────────────────────────
  services.k3s.enable = false;

  # ────────────────────────────────────────────────────────────
  # kube-identity — early-boot role broker
  # ────────────────────────────────────────────────────────────
  systemd.services.kube-identity = {
    description = "pifrost Kube Identity Broker";
    wantedBy = [ "sysinit.target" ];
    before = [ "local-fs-pre.target" "systemd-journald.service" ];
    after = [ "systemd-udevd.service" ];
    unitConfig.DefaultDependencies = false;

    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };

    script = ''
      set -eux
      KUBE_MOUNT="/mnt/kube-state"
      WORKSPACE="/run/early-kube-state"
      mkdir -p "$WORKSPACE"

      if ! mountpoint -q "$KUBE_MOUNT" 2>/dev/null; then
        mount -o ro,noload LABEL=kube-state "$KUBE_MOUNT" 2>/dev/null || true
      fi

      if [ -f "$KUBE_MOUNT/machine-id" ]; then
        cp "$KUBE_MOUNT/machine-id" "$WORKSPACE/machine-id"
        mount --bind "$WORKSPACE/machine-id" /etc/machine-id
        echo "kube-identity: machine-id bound"
      fi

      if [ -f "$KUBE_MOUNT/node-mode.env" ]; then
        source "$KUBE_MOUNT/node-mode.env"
        case "$KUBE_ROLE" in
          server)
            ${pkgs.systemd}/bin/systemctl enable k3s.service
            echo "kube-identity: enabled k3s-server"
            ;;
          agent)
            ${pkgs.systemd}/bin/systemctl enable k3s-agent.service
            echo "kube-identity: enabled k3s-agent"
            ;;
        esac
      fi

      if [ -f "$KUBE_MOUNT/etc/network/interfaces" ]; then
        cp "$KUBE_MOUNT/etc/network/interfaces" "$WORKSPACE/interfaces"
        mount --bind "$WORKSPACE/interfaces" /etc/network/interfaces 2>/dev/null || true
      fi

      echo "kube-identity: complete"
    '';
  };

  # ────────────────────────────────────────────────────────────
  # SSH — for builder access
  # ────────────────────────────────────────────────────────────
  services.openssh.enable = true;
  services.openssh.settings.PermitRootLogin = "yes";
  users.users.root.initialPassword = "packer";
  users.mutableUsers = false;

  # ────────────────────────────────────────────────────────────
  # Packages
  # ────────────────────────────────────────────────────────────
  environment.systemPackages = with pkgs; [
    curl
    rsync
    iptables
    parted
  ];

  services.resolved.enable = true;

  # ────────────────────────────────────────────────────────────
  # Disable Nix daemon — this is an appliance
  # ────────────────────────────────────────────────────────────
  nix.enable = false;
}
