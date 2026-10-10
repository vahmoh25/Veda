/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * The driver VM's renderer: the decoder (this directory) and Mesa's
 * Gallium drivers, around the program's Rust half (../src), which serves
 * the gpu protocol and runs the program.
 */

int renderer_main(int argc, char **argv);

int
main(int argc, char **argv)
{
   return renderer_main(argc, argv);
}
