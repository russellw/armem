#!/bin/sh
# Generate a batch of AArch64 litmus tests with diy7 into DIR.
#
# usage: gen_diy.sh DIR [NPROCS] [SIZE] [EDGESET]
#
# The tests are the critical cycles built from program-order, dependency,
# fence and same-location edges, combined with communication edges.
# EDGESET selects the edges:
#   basic    (default) plain, acquire/release, barrier and dependency edges
#   atomics  adds exclusives (LDXR/STXR), LSE atomics (SWP, CAS, LDADD,
#            STADD) and CSEL ("pick") dependencies
set -eu

DIR="$1"
NPROCS="${2:-2}"
SIZE="${3:-6}"
EDGESET="${4:-basic}"
DIY="${DIY7:-$HOME/.local/opt/herdtools7/usr/bin/diy7}"

case "$EDGESET" in
basic)
    EDGES="Rfe Fre Wse PodWW PodRR PodWR PodRW DpAddrdR DpAddrdW DpDatadW DpCtrldW
DpCtrlIsbdR DMB.SYdWW DMB.SYdRR DMB.SYdWR DMB.SYdRW DMB.STdWW DMB.LDdRR DMB.LDdRW
PosRR PosWR PosWW PosRW PodWWPL PodRRAP PodRWAP PodWRPA PodWRLA"
    ;;
atomics)
    EDGES="Rfe Fre Wse PodWW PodRR PodWR PodRW DpAddrdR DpAddrdW DpDatadW DpCtrldW
DMB.SYdWW DMB.LDdRR DMB.STdWW PosRR PosWR PosWW PosRW PodWWPL PodRRAP
LxSx Amo.Swp Amo.Cas Amo.LdAdd Amo.StAdd
DpAddrCseldR DpAddrCseldW DpDataCseldW DpCtrlCseldW"
    ;;
*)
    echo "unknown edge set '$EDGESET' (expected basic or atomics)" >&2
    exit 2
    ;;
esac

mkdir -p "$DIR"
cd "$DIR"
"$DIY" -arch AArch64 -nprocs "$NPROCS" -size "$SIZE" -num false -safe "$(echo $EDGES)"
