#ifndef STUB_TERMIOS_H
#define STUB_TERMIOS_H
struct termios { unsigned c_iflag, c_oflag, c_cflag, c_lflag; unsigned char c_cc[32]; };
#endif
