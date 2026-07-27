/* linux/tcp.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. TCP-заголовок (TSO/checksum-offload). */
#ifndef _LINUX_TCP_H_SHIM
#define _LINUX_TCP_H_SHIM
#include <linux/types.h>

struct tcphdr {
	__be16  source;
	__be16  dest;
	__be32  seq;
	__be32  ack_seq;
	__u16   res1:4,
		doff:4,
		fin:1,
		syn:1,
		rst:1,
		psh:1,
		ack:1,
		urg:1,
		ece:1,
		cwr:1;
	__be16  window;
	__sum16 check;
	__be16  urg_ptr;
};

#include <linux/skbuff.h>
static inline struct tcphdr *tcp_hdr(const struct sk_buff *skb)
{ return (struct tcphdr *)skb_transport_header(skb); }
static inline unsigned int tcp_hdrlen(const struct sk_buff *skb)
{ return tcp_hdr(skb)->doff * 4; }

#endif /* _LINUX_TCP_H_SHIM */
