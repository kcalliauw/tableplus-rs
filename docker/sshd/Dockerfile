# Minimal SSH bastion used by the SSH-tunnel integration tests (test-only credentials).
FROM alpine:3.20
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh tunnel \
 && echo "tunnel:tunnel_pass" | chpasswd \
 && mkdir -p /home/tunnel/.ssh
COPY test_key.pub /home/tunnel/.ssh/authorized_keys
RUN chown -R tunnel:tunnel /home/tunnel/.ssh \
 && chmod 700 /home/tunnel/.ssh && chmod 600 /home/tunnel/.ssh/authorized_keys \
 && mkdir -p /etc/ssh/sshd_config.d \
 && printf 'PasswordAuthentication yes\nPubkeyAuthentication yes\nAllowTcpForwarding yes\nPermitRootLogin no\n' > /etc/ssh/sshd_config.d/10-tunnel.conf \
 && sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /etc/ssh/sshd_config
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
