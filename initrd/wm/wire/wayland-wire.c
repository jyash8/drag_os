#include "wayland-wire.h"
#include <stdlib.h>
#include <string.h>

void wl_array_init(wl_array_t *array) {
    if (!array) return;
    array->size = 0;
    array->alloc = 0;
    array->data = NULL;
}

void wl_array_release(wl_array_t *array) {
    if (!array) return;
    if (array->data) {
        free(array->data);
        array->data = NULL;
    }
    array->size = 0;
    array->alloc = 0;
}

int wl_array_add(wl_array_t *array, const void *data, size_t size) {
    if (!array || !data || size == 0) return -1;
    if (array->size + size > array->alloc) {
        size_t new_alloc = array->alloc ? array->alloc * 2 : 64;
        while (new_alloc < array->size + size) {
            new_alloc *= 2;
        }
        void *p = realloc(array->data, new_alloc);
        if (!p) return -1;
        array->data = p;
        array->alloc = new_alloc;
    }
    memcpy((uint8_t *)array->data + array->size, data, size);
    array->size += size;
    return 0;
}

/* Connection Buffer */
void wl_conn_buffer_init(wl_connection_buffer_t *buf) {
    if (!buf) return;
    buf->head = 0;
    buf->tail = 0;
    buf->fd_head = 0;
    buf->fd_tail = 0;
}

int wl_conn_buffer_put(wl_connection_buffer_t *buf, const void *data, size_t len) {
    if (!buf || !data) return -1;
    if (buf->head + len > WL_BUFFER_CAPACITY) {
        /* If tail > 0, compact buffer */
        if (buf->tail > 0) {
            size_t remaining = buf->head - buf->tail;
            if (remaining > 0) {
                memmove(buf->data, buf->data + buf->tail, remaining);
            }
            buf->head = remaining;
            buf->tail = 0;
        }
        if (buf->head + len > WL_BUFFER_CAPACITY) {
            return -1; /* Out of space */
        }
    }
    memcpy(buf->data + buf->head, data, len);
    buf->head += len;
    return 0;
}

int wl_conn_buffer_put_fd(wl_connection_buffer_t *buf, int fd) {
    if (!buf) return -1;
    if (buf->fd_head >= 16) return -1;
    buf->fds[buf->fd_head++] = fd;
    return 0;
}

int wl_conn_buffer_get_fd(wl_connection_buffer_t *buf, int *out_fd) {
    if (!buf || !out_fd) return -1;
    if (buf->fd_tail >= buf->fd_head) return -1;
    *out_fd = buf->fds[buf->fd_tail++];
    return 0;
}

void wl_conn_buffer_consume(wl_connection_buffer_t *buf, size_t len) {
    if (!buf) return;
    buf->tail += len;
    if (buf->tail >= buf->head) {
        buf->tail = 0;
        buf->head = 0;
    }
}

int wl_conn_buffer_get_next_message(wl_connection_buffer_t *buf, const uint8_t **out_msg, size_t *out_size) {
    if (!buf || !out_msg || !out_size) return -1;
    size_t available = buf->head - buf->tail;
    if (available < 8) return 0; /* Incomplete header */

    const uint8_t *p = buf->data + buf->tail;
    uint32_t word1;
    memcpy(&word1, p + 4, 4);
    uint16_t size = (word1 >> 16) & 0xffff;

    if (size < 8) return -1; /* Corrupt message size */
    if (available < size) return 0; /* Incomplete message body */

    *out_msg = p;
    *out_size = size;
    return 1;
}

/* Message Builder */
void wl_msg_builder_init(wl_msg_builder_t *b, uint32_t object_id, uint16_t opcode) {
    if (!b) return;
    b->size = 8; /* Reserve 8 bytes for header */
    b->num_fds = 0;

    /* Write object_id */
    memcpy(b->buffer, &object_id, 4);
    /* Write opcode and placeholder size (8) */
    uint32_t word1 = ((uint32_t)8 << 16) | ((uint32_t)opcode & 0xffff);
    memcpy(b->buffer + 4, &word1, 4);
}

int wl_msg_builder_add_int(wl_msg_builder_t *b, int32_t i) {
    if (!b || b->size + 4 > WL_BUFFER_CAPACITY) return -1;
    memcpy(b->buffer + b->size, &i, 4);
    b->size += 4;
    return 0;
}

int wl_msg_builder_add_uint(wl_msg_builder_t *b, uint32_t u) {
    if (!b || b->size + 4 > WL_BUFFER_CAPACITY) return -1;
    memcpy(b->buffer + b->size, &u, 4);
    b->size += 4;
    return 0;
}

int wl_msg_builder_add_fixed(wl_msg_builder_t *b, wl_fixed_t f) {
    return wl_msg_builder_add_int(b, (int32_t)f);
}

int wl_msg_builder_add_string(wl_msg_builder_t *b, const char *s) {
    if (!b) return -1;
    if (!s) {
        return wl_msg_builder_add_uint(b, 0);
    }
    size_t slen = strlen(s) + 1; /* include null terminator */
    size_t padded_len = (slen + 3) & ~3;
    if (b->size + 4 + padded_len > WL_BUFFER_CAPACITY) return -1;

    uint32_t len_u32 = (uint32_t)slen;
    memcpy(b->buffer + b->size, &len_u32, 4);
    b->size += 4;

    memcpy(b->buffer + b->size, s, slen);
    if (padded_len > slen) {
        memset(b->buffer + b->size + slen, 0, padded_len - slen);
    }
    b->size += padded_len;
    return 0;
}

int wl_msg_builder_add_object(wl_msg_builder_t *b, uint32_t o) {
    return wl_msg_builder_add_uint(b, o);
}

int wl_msg_builder_add_new_id(wl_msg_builder_t *b, uint32_t new_id) {
    return wl_msg_builder_add_uint(b, new_id);
}

int wl_msg_builder_add_new_id_typed(wl_msg_builder_t *b, const char *interface, uint32_t version, uint32_t new_id) {
    if (wl_msg_builder_add_string(b, interface) < 0) return -1;
    if (wl_msg_builder_add_uint(b, version) < 0) return -1;
    if (wl_msg_builder_add_new_id(b, new_id) < 0) return -1;
    return 0;
}

int wl_msg_builder_add_array(wl_msg_builder_t *b, const void *data, uint32_t len) {
    if (!b) return -1;
    size_t padded_len = (len + 3) & ~3;
    if (b->size + 4 + padded_len > WL_BUFFER_CAPACITY) return -1;

    memcpy(b->buffer + b->size, &len, 4);
    b->size += 4;

    if (len > 0 && data) {
        memcpy(b->buffer + b->size, data, len);
    }
    if (padded_len > len) {
        memset(b->buffer + b->size + len, 0, padded_len - len);
    }
    b->size += padded_len;
    return 0;
}

int wl_msg_builder_add_fd(wl_msg_builder_t *b, int fd) {
    if (!b || b->num_fds >= 16) return -1;
    b->fds[b->num_fds++] = fd;
    return 0;
}

void wl_msg_builder_finish(wl_msg_builder_t *b) {
    if (!b) return;
    uint32_t word1;
    memcpy(&word1, b->buffer + 4, 4);
    uint16_t opcode = word1 & 0xffff;
    uint16_t size = (uint16_t)b->size;
    word1 = ((uint32_t)size << 16) | ((uint32_t)opcode & 0xffff);
    memcpy(b->buffer + 4, &word1, 4);
}

/* Message Reader */
int wl_msg_reader_init(wl_msg_reader_t *r, const uint8_t *data, size_t size) {
    if (!r || !data || size < 8) return -1;
    r->data = data;
    r->size = size;
    r->offset = 8;

    memcpy(&r->header.object_id, data, 4);
    uint32_t word1;
    memcpy(&word1, data + 4, 4);
    r->header.size = (word1 >> 16) & 0xffff;
    r->header.opcode = word1 & 0xffff;

    if (r->header.size != size) return -1;
    return 0;
}

int wl_msg_reader_get_int(wl_msg_reader_t *r, int32_t *out_i) {
    if (!r || !out_i || r->offset + 4 > r->size) return -1;
    memcpy(out_i, r->data + r->offset, 4);
    r->offset += 4;
    return 0;
}

int wl_msg_reader_get_uint(wl_msg_reader_t *r, uint32_t *out_u) {
    if (!r || !out_u || r->offset + 4 > r->size) return -1;
    memcpy(out_u, r->data + r->offset, 4);
    r->offset += 4;
    return 0;
}

int wl_msg_reader_get_fixed(wl_msg_reader_t *r, wl_fixed_t *out_f) {
    return wl_msg_reader_get_int(r, (int32_t *)out_f);
}

int wl_msg_reader_get_string(wl_msg_reader_t *r, const char **out_s, uint32_t *out_len) {
    if (!r || !out_s || r->offset + 4 > r->size) return -1;
    uint32_t len;
    memcpy(&len, r->data + r->offset, 4);
    r->offset += 4;

    if (len == 0) {
        *out_s = NULL;
        if (out_len) *out_len = 0;
        return 0;
    }

    size_t padded_len = (len + 3) & ~3;
    if (r->offset + padded_len > r->size) return -1;

    *out_s = (const char *)(r->data + r->offset);
    if (out_len) *out_len = len;
    r->offset += padded_len;
    return 0;
}

int wl_msg_reader_get_object(wl_msg_reader_t *r, uint32_t *out_o) {
    return wl_msg_reader_get_uint(r, out_o);
}

int wl_msg_reader_get_new_id(wl_msg_reader_t *r, uint32_t *out_new_id) {
    return wl_msg_reader_get_uint(r, out_new_id);
}

int wl_msg_reader_get_new_id_typed(wl_msg_reader_t *r, const char **out_interface, uint32_t *out_version, uint32_t *out_new_id) {
    if (wl_msg_reader_get_string(r, out_interface, NULL) < 0) return -1;
    if (wl_msg_reader_get_uint(r, out_version) < 0) return -1;
    if (wl_msg_reader_get_new_id(r, out_new_id) < 0) return -1;
    return 0;
}

int wl_msg_reader_get_array(wl_msg_reader_t *r, const void **out_data, uint32_t *out_len) {
    if (!r || !out_data || r->offset + 4 > r->size) return -1;
    uint32_t len;
    memcpy(&len, r->data + r->offset, 4);
    r->offset += 4;

    size_t padded_len = (len + 3) & ~3;
    if (r->offset + padded_len > r->size) return -1;

    *out_data = (len > 0) ? (const void *)(r->data + r->offset) : NULL;
    if (out_len) *out_len = len;
    r->offset += padded_len;
    return 0;
}

/* Helpers */
int wl_build_get_registry(wl_msg_builder_t *b, uint32_t display_id, uint32_t registry_id) {
    wl_msg_builder_init(b, display_id, WL_DISPLAY_GET_REGISTRY);
    if (wl_msg_builder_add_new_id(b, registry_id) < 0) return -1;
    wl_msg_builder_finish(b);
    return 0;
}

int wl_build_bind(wl_msg_builder_t *b, uint32_t registry_id, uint32_t name, const char *interface, uint32_t version, uint32_t new_id) {
    wl_msg_builder_init(b, registry_id, WL_REGISTRY_BIND);
    if (wl_msg_builder_add_uint(b, name) < 0) return -1;
    if (wl_msg_builder_add_new_id_typed(b, interface, version, new_id) < 0) return -1;
    wl_msg_builder_finish(b);
    return 0;
}

int wl_decode_global_event(wl_msg_reader_t *r, wl_global_event_t *out_global) {
    if (!r || !out_global) return -1;
    if (wl_msg_reader_get_uint(r, &out_global->name) < 0) return -1;
    if (wl_msg_reader_get_string(r, &out_global->interface, NULL) < 0) return -1;
    if (wl_msg_reader_get_uint(r, &out_global->version) < 0) return -1;
    return 0;
}
