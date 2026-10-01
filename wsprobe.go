// A websocket echo endpoint used only by the diagnostics launcher, to find out whether Cloud's
// edge still refuses to forward an Upgrade to a non-PHP application (it answered 520 for the Rails
// port on 2026-09-27). Deleted once the question is answered.
package main

import (
	"bufio"
	"crypto/sha1"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
)

const wsGUID = "258EAFA5-E914-47DA-95CA-5AB0DC85B11D"

func init() {
	// What the application actually receives, so a stripped Upgrade header is visible even when
	// the handshake never happens.
	http.HandleFunc("/wsprobe/headers", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"proto": r.Proto, "headers": r.Header, "tls": r.TLS != nil})
	})

	http.HandleFunc("/wsprobe", func(w http.ResponseWriter, r *http.Request) {
		key := r.Header.Get("Sec-WebSocket-Key")
		if key == "" {
			http.Error(w, "no Sec-WebSocket-Key", http.StatusBadRequest)
			return
		}
		hijacker, ok := w.(http.Hijacker)
		if !ok {
			http.Error(w, "not hijackable", http.StatusInternalServerError)
			return
		}
		conn, rw, err := hijacker.Hijack()
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		defer conn.Close()

		sum := sha1.Sum([]byte(key + wsGUID))
		fmt.Fprintf(rw, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n",
			base64.StdEncoding.EncodeToString(sum[:]))
		_ = rw.Flush()

		writeFrame(rw, []byte("hello from the app"))
		for {
			payload, err := readFrame(rw.Reader)
			if err != nil {
				return
			}
			writeFrame(rw, append([]byte("echo: "), payload...))
		}
	})
}

// writeFrame writes one unmasked text frame, which is all a server may send.
func writeFrame(rw *bufio.ReadWriter, payload []byte) {
	header := []byte{0x81}
	switch n := len(payload); {
	case n < 126:
		header = append(header, byte(n))
	default:
		header = append(header, 126, byte(n>>8), byte(n))
	}
	_, _ = rw.Write(header)
	_, _ = rw.Write(payload)
	_ = rw.Flush()
}

// readFrame reads one masked client frame, ignoring fragmentation and control frames beyond close.
func readFrame(r *bufio.Reader) ([]byte, error) {
	head := make([]byte, 2)
	if _, err := ioReadFull(r, head); err != nil {
		return nil, err
	}
	if head[0]&0x0f == 0x8 {
		return nil, fmt.Errorf("close")
	}
	length := int(head[1] & 0x7f)
	switch length {
	case 126:
		ext := make([]byte, 2)
		if _, err := ioReadFull(r, ext); err != nil {
			return nil, err
		}
		length = int(ext[0])<<8 | int(ext[1])
	case 127:
		return nil, fmt.Errorf("frame too large for a probe")
	}
	mask := make([]byte, 4)
	if head[1]&0x80 != 0 {
		if _, err := ioReadFull(r, mask); err != nil {
			return nil, err
		}
	}
	payload := make([]byte, length)
	if _, err := ioReadFull(r, payload); err != nil {
		return nil, err
	}
	if head[1]&0x80 != 0 {
		for i := range payload {
			payload[i] ^= mask[i%4]
		}
	}
	return payload, nil
}

func ioReadFull(r *bufio.Reader, buf []byte) (int, error) {
	read := 0
	for read < len(buf) {
		n, err := r.Read(buf[read:])
		read += n
		if err != nil {
			return read, err
		}
	}
	return read, nil
}
