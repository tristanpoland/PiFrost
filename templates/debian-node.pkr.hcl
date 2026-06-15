packer {
  required_plugins {
    qemu = {
      version = ">= 1.1.0"
      source  = "github.com/hashicorp/qemu"
    }
  }
}

variable "kube_uuid" {
  type        = string
  default     = "deadbeef-1234-5678-9abc-def012345678"
  description = "UUID for the persistent kube-state partition"
}

variable "arch" {
  type        = string
  default     = "amd64"
  description = "Target CPU architecture (amd64 or arm64)"
}

variable "output_dir" {
  type        = string
  default     = "./output"
  description = "Directory for the final .img artifact"
}

locals {
  iso_url = var.arch == "arm64" ? (
    "https://cdimage.debian.org/debian-cd/current/arm64/iso-cd/debian-12.5.0-arm64-netinst.iso"
  ) : (
    "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-12.5.0-amd64-netinst.iso"
  )

  iso_checksum = var.arch == "arm64" ? (
    "sha256:3b7a7a9b5c6d8f0e1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f"
  ) : (
    "sha256:a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2"
  )

  http_dir = abspath("${path.root}/http")
}

source "qemu" "debian-kube" {
  accelerator = "kvm"
  boot_command = [
    "<esc><wait>",
    "install ",
    "preseed/url=http://{{ .HTTPIP }}:{{ .HTTPPort }}/preseed.cfg ",
    "auto=true ",
    "priority=critical ",
    "net.ifnames=0 ",
    "biosdevname=0 ",
    "---<enter>"
  ]
  boot_key_interval = "10ms"
  boot_wait         = "10s"

  disk_image        = false
  disk_size         = "6G"
  disk_compression  = true
  disk_interface    = "virtio-scsi"
  format            = "raw"

  headless          = true
  http_directory    = local.http_dir

  iso_checksum      = local.iso_checksum
  iso_url           = local.iso_url

  memory            = 2048
  cpus              = 2

  output_directory  = var.output_dir
  qemu_binary       = var.arch == "arm64" ? "qemu-system-aarch64" : "qemu-system-x86_64"

  qemuargs = var.arch == "arm64" ? [
    ["-machine", "virt,gic-version=max"],
    ["-cpu", "cortex-a72"],
  ] : [
    ["-machine", "q35"],
    ["-cpu", "host"],
  ]

  shutdown_command = "echo 'packer' | sudo -S shutdown -P now"
  ssh_password     = "packer"
  ssh_username     = "root"
  ssh_timeout      = "30m"

  net_device        = "virtio-net"
  use_default_display = false
  vnc_bind_address  = "127.0.0.1"
}

build {
  sources = ["source.qemu.debian-kube"]

  provisioner "shell" {
    inline = [
      "echo '=== pifrost: Debian 12 post-install environment ready ==='",
      "uname -a",
      "cat /etc/debian_version",
    ]
  }

  provisioner "shell" {
    environment_vars = ["DEBIAN_FRONTEND=noninteractive"]
    inline = [
      "apt-get update -qq",
      "apt-get install -y -qq \
        overlayroot \
        curl \
        rsync \
        iptables \
        systemd-resolved \
        ca-certificates \
        gnupg \
        lsb-release \
        parted \
        sudo \
        xxd \
        dbus",
      "apt-get clean",
    ]
  }

  provisioner "shell" {
    inline = [
      <<-EOF
        if [ -f /etc/default/grub ]; then
          sed -i 's/^GRUB_CMDLINE_LINUX=""/GRUB_CMDLINE_LINUX="cgroup_enable=cpuset cgroup_enable=memory cgroup_memory=1"/' /etc/default/grub
          sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT="quiet"/GRUB_CMDLINE_LINUX_DEFAULT="quiet cgroup_enable=cpuset cgroup_enable=memory cgroup_memory=1"/' /etc/default/grub
          update-grub
        fi
        if [ -f /boot/firmware/cmdline.txt ]; then
          sed -i 's/$/ cgroup_enable=cpuset cgroup_enable=memory cgroup_memory=1/' /boot/firmware/cmdline.txt
        fi
      EOF
    ]
  }

  provisioner "shell" {
    inline = [
      "curl -sfL https://get.k3s.io -o /usr/local/bin/k3s-install.sh",
      "chmod 755 /usr/local/bin/k3s-install.sh",

      "INSTALL_K3S_SKIP_ENABLE=true INSTALL_K3S_SKIP_START=true INSTALL_K3S_EXEC=server /usr/local/bin/k3s-install.sh 2>&1 || true",
      "INSTALL_K3S_SKIP_ENABLE=true INSTALL_K3S_SKIP_START=true INSTALL_K3S_EXEC=agent /usr/local/bin/k3s-install.sh 2>&1 || true",

      "rm -rf /var/lib/rancher/k3s /var/lib/containerd /etc/rancher/k3s 2>/dev/null || true",

      "install -d -m 755 /mnt/kube-state",
    ]
  }

  provisioner "shell" {
    inline = [
      "ln -sf /mnt/kube-state/k3s /var/lib/rancher/k3s",
      "ln -sf /mnt/kube-state/containerd /var/lib/containerd",
      "ln -sf /mnt/kube-state/etc/k3s /etc/rancher/k3s",
    ]
  }

  provisioner "shell" {
    inline = [
      "rm -f /etc/machine-id /var/lib/dbus/machine-id",
      "touch /etc/machine-id",
      "install -d -m 755 /var/lib/dbus",
    ]
  }

  provisioner "file" {
    content = <<-SH
#!/bin/bash
set -euo pipefail

KUBE_PART_UUID="${var.kube_uuid}"
WORKSPACE="/run/early-kube-state"
KUBE_MOUNT="/mnt/kube-state"

mkdir -p "$WORKSPACE"

if ! mountpoint -q "$KUBE_MOUNT" 2>/dev/null; then
  if [ -n "$KUBE_PART_UUID" ] && [ "$KUBE_PART_UUID" != "none" ]; then
    mount -o ro,noload UUID="$KUBE_PART_UUID" "$KUBE_MOUNT" 2>/dev/null || \
      mount -o ro,noload LABEL=kube-state "$KUBE_MOUNT" 2>/dev/null || \
      echo "WARNING: kube-state partition not found"
  else
    mount -o ro,noload LABEL=kube-state "$KUBE_MOUNT" 2>/dev/null || \
      echo "WARNING: kube-state partition not found"
  color: #b2ceee">fi
fi

if [ -f "$KUBE_MOUNT/machine-id" ]; then
  cp "$KUBE_MOUNT/machine-id" "$WORKSPACE/machine-id"
  mount --bind "$WORKSPACE/machine-id" /etc/machine-id
  if [ -d /var/lib/dbus ]; then
    cp "$KUBE_MOUNT/machine-id" "$WORKSPACE/dbus-machine-id" 2>/dev/null || true
    mount --bind "$WORKSPACE/dbus-machine-id" /var/lib/dbus/machine-id 2>/dev/null || true
  fi
  echo "kube-identity: machine-id bound from kube-state"
else
  echo "WARNING: No machine-id on kube-state"
fi

if [ -f "$KUBE_MOUNT/node-mode.env" ]; then
  source "$KUBE_MOUNT/node-mode.env"
  case "${KUBE_ROLE:-}" in
    server)
      systemctl enable k3s-server.service
      echo "kube-identity: Enabled k3s-server"
      ;;
    agent)
      systemctl enable k3s-agent.service
      echo "kube-identity: Enabled k3s-agent"
      ;;
    *)
      echo "WARNING: Unknown KUBE_ROLE='${KUBE_ROLE:-}'"
      ;;
  esac
fi

if [ -d "$KUBE_MOUNT/etc/network" ]; then
  mkdir -p /run/systemd/network 2>/dev/null || true
  if [ -d "$KUBE_MOUNT/etc/network/systemd-networkd" ]; then
    cp -r "$KUBE_MOUNT/etc/network/systemd-networkd/"* /run/systemd/network/ 2>/dev/null || true
  fi
  if [ -f "$KUBE_MOUNT/etc/network/interfaces" ]; then
    cp "$KUBE_MOUNT/etc/network/interfaces" "$WORKSPACE/interfaces"
    mount --bind "$WORKSPACE/interfaces" /etc/network/interfaces 2>/dev/null || true
  fi
fi

echo "kube-identity: completed successfully"
SH
    destination = "/lib/systemd/systemd-kube-identity"
  }

  provisioner "shell" {
    inline = ["chmod 755 /lib/systemd/systemd-kube-identity"]
  }

  provisioner "file" {
    content = <<-UNIT
[Unit]
Description=pifrost Kube Identity Broker
DefaultDependencies=no
Before=local-fs-pre.target systemd-journald.service
After=systemd-udevd.service

[Service]
Type=oneshot
ExecStart=/lib/systemd/systemd-kube-identity
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=sysinit.target
UNIT
    destination = "/etc/systemd/system/kube-identity.service"
  }

  provisioner "shell" {
    inline = ["systemctl enable kube-identity.service"]
  }

  provisioner "shell" {
    inline = [
      <<-EOF
        FS_LINE="UUID=${var.kube_uuid}  /mnt/kube-state  ext4  rw,noatime,nodiratime,errors=remount-ro  0 2"
        if ! grep -q "kube-state" /etc/fstab 2>/dev/null; then
          echo "$FS_LINE" >> /etc/fstab
        fi
      EOF
    ]
  }

  provisioner "file" {
    content = <<-OVR
overlayroot="tmpfs:recurse=0"
overlayroot_cfg_exclude=["/mnt/kube-state"]
OVR
    destination = "/etc/overlayroot.conf"
  }

  provisioner "shell" {
    inline = [
      "systemctl enable systemd-resolved",
      "ln -sf /run/systemd/resolve/stub-resolv.conf /etc/resolv.conf",
    ]
  }

  provisioner "shell" {
    inline = [
      <<-EOF
        cat > /etc/modules-load.d/k3s.conf << 'MOD'
overlay
br_netfilter
nf_conntrack
nf_tables
MOD

        cat > /etc/sysctl.d/99-k3s.conf << 'SYS'
net.ipv4.ip_forward=1
net.ipv6.conf.all.forwarding=1
net.bridge.bridge-nf-call-iptables=1
net.bridge.bridge-nf-call-ip6tables=1
net.ipv4.conf.all.rp_filter=1
kernel.panic=10
kernel.panic_on_oops=1
vm.overcommit_memory=1
SYS
      EOF
    ]
  }

  provisioner "shell" {
    inline = [
      "apt-get clean",
      "rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*",
      "dd if=/dev/zero of=/zero.fill bs=1M || true",
      "rm -f /zero.fill",
      "sync",
    ]
  }
}
