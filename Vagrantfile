# -*- mode: ruby -*-
# vi: set ft=ruby :

BOX = "generic/ubuntu2204"
VMS = {
  "first" => "192.168.56.4",
  "second" => "192.168.56.5"
}

Vagrant.configure("2") do |config|
  # The most common configuration options are documented and commented below.
  # For a complete reference, please see the online documentation at
  # https://docs.vagrantup.com.
  VMS.each do |name, address|
    config.vm.define name do |vm|
      vm.vm.box = BOX
      vm.vm.synced_folder "./target", "/target"
      vm.vm.network "private_network", ip: address
      vm.vm.provision "shell", inline: <<-SHELL
        modprobe br_netfilter
      SHELL
    end
  end
end
