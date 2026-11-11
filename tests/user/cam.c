/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * cam: checks CAM's user interfaces and the NVMe control devices,
 * read-only, as libcam, camcontrol(8), nvmecontrol(8), and smartmontools
 * use them:
 *
 * - /dev/xpt0 is root:operator 0600 and opens only read-write;
 *   XPT_DEV_MATCH lists the table (the transport layer's own bus last,
 *   with its xpt0 peripheral), the same a result at a time as all at
 *   once; a peripheral pattern finds xpt0; XPT_PATH_INQ on its bus
 *   answers; CAMGETPASSTHRU refuses names without a pass device.
 * - Each pass device answers CAMGETPASSTHRU with itself, XPT_GDEV_TYPE,
 *   XPT_PATH_INQ, and XPT_GET_TRAN_SETTINGS; refuses transport-only
 *   requests (ENODEV); and runs its protocol's commands: INQUIRY, TEST
 *   UNIT READY, and SAT's ATA PASS-THROUGH(16) (which may be refused
 *   with sense data) for SCSI; IDENTIFY DEVICE and SMART RETURN STATUS
 *   for ATA; IDENTIFY over XPT_NVME_ADMIN, matching XPT_DEV_ADVINFO's
 *   copy, and a READ over XPT_NVME_IO for NVMe.
 * - /dev/nvme0 (if there is one) is root:wheel 0600 and answers
 *   NVME_PASSTHROUGH_CMD (identify, the SMART/health log page, a READ on
 *   its namespace node matching the node's own read), NVME_GET_NSID,
 *   NVME_GET_MAX_XFER_SIZE, NVME_GET_CONTROLLER_DATA; a transfer past
 *   the largest fails with EIO, a buffer it cannot hold with ENOMEM.
 *
 * Runs on FreeBSD too (as root). Exits 0 on success; prints what failed
 * otherwise.
 */

#include <sys/param.h>
#include <sys/disk.h>
#include <sys/ioctl.h>
#include <sys/stat.h>

#include <cam/cam.h>
#include <cam/cam_ccb.h>
#include <cam/scsi/scsi_all.h>
#include <cam/scsi/scsi_pass.h>
#include <dev/nvme/nvme.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("cam: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	MAX_RESULTS	128

/* XPT_DEV_MATCH from `pos`, `room` results at a time. */
static int
dev_match(int fd, struct dev_match_pattern *pats, int npats,
    struct dev_match_result *out, int room, int max)
{
	union ccb ccb;
	int n = 0;

	memset(&ccb, 0, sizeof(ccb));
	ccb.ccb_h.func_code = XPT_DEV_MATCH;
	ccb.ccb_h.path_id = CAM_XPT_PATH_ID;
	ccb.ccb_h.target_id = CAM_TARGET_WILDCARD;
	ccb.ccb_h.target_lun = CAM_LUN_WILDCARD;
	ccb.cdm.num_patterns = npats;
	ccb.cdm.pattern_buf_len = npats * sizeof(*pats);
	ccb.cdm.patterns = pats;
	ccb.cdm.match_buf_len = room * sizeof(struct dev_match_result);
	do {
		ccb.cdm.matches = &out[n];
		if (n + room > max)
			return (-1);
		if (ioctl(fd, CAMIOCOMMAND, &ccb) == -1)
			return (-1);
		if (ccb.ccb_h.status != CAM_REQ_CMP ||
		    (ccb.cdm.status != CAM_DEV_MATCH_LAST &&
		    ccb.cdm.status != CAM_DEV_MATCH_MORE))
			return (-1);
		n += ccb.cdm.num_matches;
	} while (ccb.cdm.status == CAM_DEV_MATCH_MORE);
	return (n);
}

/* A SCSI command's CCB: its data, its CDB's length (filled by the caller). */
static void
scsi_cmd(union ccb *ccb, union ccb *base, uint32_t dir, uint8_t *data,
    uint32_t len, uint8_t cdb_len)
{
	*ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb->csio);
	ccb->ccb_h.func_code = XPT_SCSI_IO;
	ccb->ccb_h.flags = dir | CAM_DEV_QFRZDIS;
	ccb->ccb_h.timeout = 5000;
	ccb->csio.data_ptr = data;
	ccb->csio.dxfer_len = len;
	ccb->csio.sense_len = SSD_FULL_SIZE;
	ccb->csio.cdb_len = cdb_len;
}

static void
check_scsi(int fd, union ccb *base)
{
	struct scsi_inquiry_data inq;
	union ccb ccb;
	uint8_t buf[512];

	/* INQUIRY, as the probe saw it. */
	memset(&inq, 0, sizeof(inq));
	scsi_cmd(&ccb, base, CAM_DIR_IN, (uint8_t *)&inq,
	    SHORT_INQUIRY_LENGTH, 6);
	ccb.csio.cdb_io.cdb_bytes[0] = INQUIRY;
	ccb.csio.cdb_io.cdb_bytes[4] = SHORT_INQUIRY_LENGTH;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "INQUIRY: %s",
	    strerror(errno));
	CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP,
	    "INQUIRY: status %#x", ccb.ccb_h.status);
	CHECK(memcmp(inq.vendor, base->cgd.inq_data.vendor,
	    sizeof(inq.vendor)) == 0, "INQUIRY's vendor is not GDEV_TYPE's");

	/* TEST UNIT READY: no data; ready, or not with sense data. */
	scsi_cmd(&ccb, base, CAM_DIR_NONE, NULL, 0, 6);
	ccb.csio.cdb_io.cdb_bytes[0] = TEST_UNIT_READY;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "TUR: %s", strerror(errno));
	CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP ||
	    ((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_SCSI_STATUS_ERROR &&
	    (ccb.ccb_h.status & CAM_AUTOSNS_VALID)), "TUR: status %#x",
	    ccb.ccb_h.status);

	/*
	 * SAT's ATA PASS-THROUGH(16), IDENTIFY DEVICE: done, refused with
	 * sense data (ILLEGAL REQUEST from a disk without SAT), or refused by
	 * the SIM.
	 */
	scsi_cmd(&ccb, base, CAM_DIR_IN, buf, sizeof(buf), 16);
	ccb.csio.cdb_io.cdb_bytes[0] = 0x85;	/* ATA PASS-THROUGH(16) */
	ccb.csio.cdb_io.cdb_bytes[1] = 4 << 1;	/* PIO data-in */
	ccb.csio.cdb_io.cdb_bytes[2] = 0x0e;	/* T_DIR, BYT_BLOK, sectors */
	ccb.csio.cdb_io.cdb_bytes[6] = 1;
	ccb.csio.cdb_io.cdb_bytes[14] = 0xec;	/* IDENTIFY DEVICE */
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "SAT: %s", strerror(errno));
	if ((ccb.ccb_h.status & CAM_STATUS_MASK) != CAM_REQ_CMP &&
	    (ccb.ccb_h.status & CAM_STATUS_MASK) != CAM_REQ_INVALID) {
		CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) ==
		    CAM_SCSI_STATUS_ERROR &&
		    (ccb.ccb_h.status & CAM_AUTOSNS_VALID) &&
		    ccb.csio.scsi_status == SCSI_STATUS_CHECK_COND &&
		    (ccb.csio.sense_data.error_code & 0x7f) >= 0x70,
		    "SAT refused without sense: status %#x scsi %#x",
		    ccb.ccb_h.status, ccb.csio.scsi_status);
	}
}

static void
check_ata(int fd, union ccb *base)
{
	struct ata_params id;
	union ccb ccb;

	/* IDENTIFY DEVICE, PIO in. */
	memset(&id, 0, sizeof(id));
	ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.ataio);
	ccb.ccb_h.func_code = XPT_ATA_IO;
	ccb.ccb_h.flags = CAM_DIR_IN | CAM_DEV_QFRZDIS;
	ccb.ccb_h.timeout = 5000;
	ccb.ataio.data_ptr = (uint8_t *)&id;
	ccb.ataio.dxfer_len = sizeof(id);
	ccb.ataio.cmd.command = 0xec;
	ccb.ataio.cmd.sector_count = 1;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "IDENTIFY: %s",
	    strerror(errno));
	CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP,
	    "IDENTIFY: status %#x", ccb.ccb_h.status);
	CHECK(ccb.ataio.resid == 0, "IDENTIFY: resid %u", ccb.ataio.resid);
	CHECK(id.lba_size48_1 != 0 || id.lba_size48_2 != 0 ||
	    id.lba_size_1 != 0, "IDENTIFY: no size");

	/* SMART RETURN STATUS: the result registers. */
	ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.ataio);
	ccb.ccb_h.func_code = XPT_ATA_IO;
	ccb.ccb_h.flags = CAM_DIR_NONE | CAM_DEV_QFRZDIS;
	ccb.ccb_h.timeout = 5000;
	ccb.ataio.cmd.flags = CAM_ATAIO_NEEDRESULT;
	ccb.ataio.cmd.command = 0xb0;
	ccb.ataio.cmd.features = 0xda;
	ccb.ataio.cmd.lba_mid = 0x4f;
	ccb.ataio.cmd.lba_high = 0xc2;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "SMART: %s",
	    strerror(errno));
	if ((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP)
		CHECK(ccb.ataio.res.lba_mid == 0x4f ||
		    ccb.ataio.res.lba_mid == 0xf4, "SMART: lba_mid %#x",
		    ccb.ataio.res.lba_mid);
}

static void
check_nvme(int fd, union ccb *base)
{
	static struct nvme_controller_data cd, adv;
	static uint8_t blk[4096];
	union ccb ccb;

	memset(&cd, 0, sizeof(cd));
	ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.nvmeio);
	ccb.ccb_h.func_code = XPT_NVME_ADMIN;
	ccb.ccb_h.flags = CAM_DIR_IN;
	ccb.nvmeio.cmd.opc = NVME_OPC_IDENTIFY;
	ccb.nvmeio.cmd.cdw10 = htole32(1);
	ccb.nvmeio.data_ptr = (uint8_t *)&cd;
	ccb.nvmeio.dxfer_len = sizeof(cd);
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "NVMe identify: %s",
	    strerror(errno));
	CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP,
	    "NVMe identify: status %#x", ccb.ccb_h.status);

	memset(&adv, 0, sizeof(adv));
	ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.cdai);
	ccb.ccb_h.func_code = XPT_DEV_ADVINFO;
	ccb.ccb_h.flags = CAM_DIR_IN;
	ccb.cdai.buftype = CDAI_TYPE_NVME_CNTRL;
	ccb.cdai.bufsiz = sizeof(adv);
	ccb.cdai.buf = (uint8_t *)&adv;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "ADVINFO: %s",
	    strerror(errno));
	CHECK(ccb.ccb_h.status == CAM_REQ_CMP && ccb.cdai.provsiz ==
	    sizeof(adv), "ADVINFO: status %#x size %jd", ccb.ccb_h.status,
	    (intmax_t)ccb.cdai.provsiz);
	CHECK(memcmp(cd.sn, adv.sn, sizeof(cd.sn)) == 0 &&
	    memcmp(cd.mn, adv.mn, sizeof(cd.mn)) == 0,
	    "ADVINFO's identify data is not the controller's");

	/* A READ of the path's namespace's first block. */
	ccb = *base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.nvmeio);
	ccb.ccb_h.func_code = XPT_NVME_IO;
	ccb.ccb_h.flags = CAM_DIR_IN;
	ccb.nvmeio.cmd.opc = NVME_OPC_READ;
	ccb.nvmeio.cmd.nsid = htole32(base->ccb_h.target_lun);
	ccb.nvmeio.data_ptr = blk;
	ccb.nvmeio.dxfer_len = 512;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0, "NVMe read: %s",
	    strerror(errno));
	CHECK((ccb.ccb_h.status & CAM_STATUS_MASK) == CAM_REQ_CMP,
	    "NVMe read: status %#x", ccb.ccb_h.status);
}

/* What a pass device answers. */
static void
check_pass(int unit)
{
	char path[64];
	struct stat st;
	union ccb base, ccb;
	int fd;

	snprintf(path, sizeof(path), "/dev/pass%d", unit);
	CHECK(stat(path, &st) == 0 && S_ISCHR(st.st_mode) &&
	    (st.st_mode & 07777) == 0600 && st.st_uid == 0 && st.st_gid == 5,
	    "%s: mode %o owner %u:%u", path, st.st_mode, st.st_uid,
	    st.st_gid);
	fd = open(path, O_RDWR);
	CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
	if (fd < 0)
		return;

	memset(&ccb, 0, sizeof(ccb));
	ccb.ccb_h.func_code = XPT_GDEVLIST;
	CHECK(ioctl(fd, CAMGETPASSTHRU, &ccb) == 0, "%s: GETPASSTHRU: %s",
	    path, strerror(errno));
	CHECK(strcmp(ccb.cgdl.periph_name, "pass") == 0 &&
	    ccb.cgdl.unit_number == (u_int)unit, "%s: GETPASSTHRU says %s%u",
	    path, ccb.cgdl.periph_name, ccb.cgdl.unit_number);
	base = ccb;

	ccb = base;
	ccb.ccb_h.func_code = XPT_GDEV_TYPE;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0 &&
	    ccb.ccb_h.status == CAM_REQ_CMP, "%s: GDEV_TYPE: %#x", path,
	    ccb.ccb_h.status);
	base.cgd = ccb.cgd;

	ccb = base;
	ccb.ccb_h.func_code = XPT_PATH_INQ;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0 &&
	    ccb.ccb_h.status == CAM_REQ_CMP && ccb.cpi.dev_name[0] != '\0' &&
	    strcmp(ccb.cpi.sim_vid, "FreeBSD") == 0, "%s: PATH_INQ: %#x",
	    path, ccb.ccb_h.status);

	ccb = base;
	CCB_CLEAR_ALL_EXCEPT_HDR(&ccb.cts);
	ccb.ccb_h.func_code = XPT_GET_TRAN_SETTINGS;
	ccb.cts.type = CTS_TYPE_CURRENT_SETTINGS;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0 &&
	    (ccb.ccb_h.status == CAM_REQ_INVALID ||
	    (ccb.ccb_h.status == CAM_REQ_CMP &&
	    ccb.cts.protocol == base.cgd.protocol)), "%s: GET_TRAN_SETTINGS: "
	    "%#x protocol %d", path, ccb.ccb_h.status, ccb.cts.protocol);

	/* Only the transport layer's device takes a bus scan. */
	ccb = base;
	ccb.ccb_h.func_code = XPT_SCAN_BUS;
	errno = 0;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == -1 && errno == ENODEV,
	    "%s: SCAN_BUS: %s", path, strerror(errno));

	switch (base.cgd.protocol) {
	case PROTO_SCSI:
		check_scsi(fd, &base);
		break;
	case PROTO_ATA:
		check_ata(fd, &base);
		break;
	case PROTO_NVME:
		check_nvme(fd, &base);
		break;
	default:
		break;
	}
	close(fd);
}

static int
pt(int fd, uint8_t opc, uint32_t nsid, uint32_t cdw10, void *buf,
    uint32_t len, struct nvme_pt_command *out)
{
	memset(out, 0, sizeof(*out));
	out->cmd.opc = opc;
	out->cmd.nsid = htole32(nsid);
	out->cmd.cdw10 = htole32(cdw10);
	out->buf = buf;
	out->len = len;
	out->is_read = 1;
	return (ioctl(fd, NVME_PASSTHROUGH_CMD, out));
}

static void
check_nvme_nodes(void)
{
	static struct nvme_controller_data cd, cd2;
	static struct nvme_namespace_data ns;
	static uint8_t log[512], blk[4096], via_node[4096];
	struct nvme_pt_command p;
	struct nvme_get_nsid gn;
	struct stat st;
	uint64_t max;
	off_t size;
	int fd, nfd;

	if (stat("/dev/nvme0", &st) != 0)
		return;
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0600 &&
	    st.st_uid == 0 && st.st_gid == 0, "/dev/nvme0: mode %o owner "
	    "%u:%u", st.st_mode, st.st_uid, st.st_gid);
	fd = open("/dev/nvme0", O_RDONLY);
	CHECK(fd >= 0, "open /dev/nvme0: %s", strerror(errno));
	if (fd < 0)
		return;
	CHECK(pt(fd, NVME_OPC_IDENTIFY, 0, 1, &cd, sizeof(cd), &p) == 0 &&
	    !nvme_completion_is_error(&p.cpl), "identify: %s status %#x",
	    strerror(errno), p.cpl.status);
	CHECK(ioctl(fd, NVME_GET_CONTROLLER_DATA, &cd2) == 0 &&
	    memcmp(&cd, &cd2, sizeof(cd)) == 0,
	    "GET_CONTROLLER_DATA is not identify's");
	CHECK(ioctl(fd, NVME_GET_NSID, &gn) == 0 &&
	    strcmp(gn.cdev, "nvme0") == 0 && gn.nsid == 0, "GET_NSID: %s %u",
	    gn.cdev, gn.nsid);
	CHECK(ioctl(fd, NVME_GET_MAX_XFER_SIZE, &max) == 0 && max >= 4096,
	    "GET_MAX_XFER_SIZE: %ju", (uintmax_t)max);
	/* The SMART / health information log page. */
	CHECK(pt(fd, NVME_OPC_GET_LOG_PAGE, NVME_GLOBAL_NAMESPACE_TAG,
	    NVME_LOG_HEALTH_INFORMATION | ((sizeof(log) / 4 - 1) << 16), log,
	    sizeof(log), &p) == 0 && !nvme_completion_is_error(&p.cpl),
	    "SMART log: %s status %#x", strerror(errno), p.cpl.status);
	/* Past the largest transfer; and a buffer not mapped. */
	errno = 0;
	CHECK(pt(fd, NVME_OPC_IDENTIFY, 0, 1, NULL, (uint32_t)max + 4096,
	    &p) == -1 && errno == EIO, "a transfer too long: %s",
	    strerror(errno));
	errno = 0;
	CHECK(pt(fd, NVME_OPC_IDENTIFY, 0, 1, (void *)(uintptr_t)8,
	    sizeof(cd), &p) == -1 && errno == ENOMEM, "a bad buffer: %s",
	    strerror(errno));

	nfd = open("/dev/nvme0ns1", O_RDONLY);
	CHECK(nfd >= 0, "open /dev/nvme0ns1: %s", strerror(errno));
	if (nfd >= 0) {
		CHECK(ioctl(nfd, NVME_GET_NSID, &gn) == 0 &&
		    strcmp(gn.cdev, "nvme0") == 0 && gn.nsid == 1,
		    "ns GET_NSID: %s %u", gn.cdev, gn.nsid);
		CHECK(pt(fd, NVME_OPC_IDENTIFY, 1, 0, &ns, sizeof(ns), &p) ==
		    0 && !nvme_completion_is_error(&p.cpl), "identify ns: %s",
		    strerror(errno));
		CHECK(ioctl(nfd, DIOCGMEDIASIZE, &size) == 0 && size ==
		    (off_t)(ns.nsze * (1 << NVMEV(NVME_NS_DATA_LBAF_LBADS,
		    ns.lbaf[NVMEV(NVME_NS_DATA_FLBAS_FORMAT, ns.flbas)]))),
		    "media size %jd", (intmax_t)size);
		/* A READ through the namespace: the node's own bytes. */
		CHECK(pt(nfd, NVME_OPC_READ, 0, 0, blk, 512, &p) == 0 &&
		    !nvme_completion_is_error(&p.cpl), "read: %s status %#x",
		    strerror(errno), p.cpl.status);
		CHECK(pread(nfd, via_node, 512, 0) == 512 &&
		    memcmp(blk, via_node, 512) == 0,
		    "pass-through READ differs from the node's");
		close(nfd);
	}
	close(fd);
}

int
main(void)
{
	static struct dev_match_result all[MAX_RESULTS], one[MAX_RESULTS];
	struct dev_match_pattern pat;
	struct periph_match_result *pr;
	union ccb ccb;
	struct stat st;
	int fd, n, m, i;

	CHECK(stat("/dev/xpt0", &st) == 0 && S_ISCHR(st.st_mode) &&
	    (st.st_mode & 07777) == 0600 && st.st_uid == 0 && st.st_gid == 5,
	    "/dev/xpt0: mode %o owner %u:%u", st.st_mode, st.st_uid,
	    st.st_gid);
	errno = 0;
	fd = open("/dev/xpt0", O_RDONLY);
	CHECK(fd == -1 && errno == EPERM, "xpt0 opened read-only: %s",
	    strerror(errno));
	if (fd >= 0)
		close(fd);
	fd = open("/dev/xpt0", O_RDWR);
	CHECK(fd >= 0, "open /dev/xpt0: %s", strerror(errno));
	if (fd < 0)
		return (1);

	/* Everything, then a result at a time: the same. */
	n = dev_match(fd, NULL, 0, all, MAX_RESULTS, MAX_RESULTS);
	CHECK(n >= 3, "DEV_MATCH: %d results: %s", n, strerror(errno));
	m = dev_match(fd, NULL, 0, one, 1, MAX_RESULTS);
	CHECK(m == n && memcmp(all, one, n * sizeof(all[0])) == 0,
	    "paged DEV_MATCH differs: %d results, not %d", m, n);
	if (n >= 3) {
		CHECK(all[n - 3].type == DEV_MATCH_BUS &&
		    all[n - 3].result.bus_result.path_id == CAM_XPT_PATH_ID &&
		    strcmp(all[n - 3].result.bus_result.dev_name, "xpt") == 0,
		    "the last bus is not xpt's");
		CHECK(all[n - 2].type == DEV_MATCH_DEVICE &&
		    (all[n - 2].result.device_result.flags &
		    DEV_RESULT_UNCONFIGURED), "xpt's device is configured");
		pr = &all[n - 1].result.periph_result;
		CHECK(all[n - 1].type == DEV_MATCH_PERIPH &&
		    strcmp(pr->periph_name, "xpt") == 0 && pr->unit_number == 0,
		    "the last peripheral is not xpt0");
	}

	/* A peripheral pattern: xpt0, from the drivers' lists. */
	memset(&pat, 0, sizeof(pat));
	pat.type = DEV_MATCH_PERIPH;
	strlcpy(pat.pattern.periph_pattern.periph_name, "xpt",
	    sizeof(pat.pattern.periph_pattern.periph_name));
	pat.pattern.periph_pattern.flags = PERIPH_MATCH_NAME;
	m = dev_match(fd, &pat, 1, one, 4, MAX_RESULTS);
	CHECK(m == 1 && one[0].type == DEV_MATCH_PERIPH &&
	    one[0].result.periph_result.path_id == CAM_XPT_PATH_ID &&
	    one[0].result.periph_result.target_id == CAM_TARGET_WILDCARD,
	    "xpt0 by name: %d results", m);

	/* The transport layer's bus. */
	memset(&ccb, 0, sizeof(ccb));
	ccb.ccb_h.func_code = XPT_PATH_INQ;
	ccb.ccb_h.path_id = CAM_XPT_PATH_ID;
	ccb.ccb_h.target_id = CAM_TARGET_WILDCARD;
	ccb.ccb_h.target_lun = CAM_LUN_WILDCARD;
	CHECK(ioctl(fd, CAMIOCOMMAND, &ccb) == 0 &&
	    ccb.ccb_h.status == CAM_REQ_CMP &&
	    strcmp(ccb.cpi.dev_name, "xpt") == 0 &&
	    strcmp(ccb.cpi.sim_vid, "FreeBSD") == 0, "xpt PATH_INQ: %#x %s",
	    ccb.ccb_h.status, ccb.cpi.dev_name);

	/* No pass device for xpt0; no such driver; no name. */
	memset(&ccb, 0, sizeof(ccb));
	strlcpy(ccb.cgdl.periph_name, "xpt", sizeof(ccb.cgdl.periph_name));
	errno = 0;
	CHECK(ioctl(fd, CAMGETPASSTHRU, &ccb) == -1 && errno == ENOENT,
	    "GETPASSTHRU xpt0: %s", strerror(errno));
	strlcpy(ccb.cgdl.periph_name, "nosuch", sizeof(ccb.cgdl.periph_name));
	errno = 0;
	CHECK(ioctl(fd, CAMGETPASSTHRU, &ccb) == -1 && errno == ENOENT,
	    "GETPASSTHRU nosuch0: %s", strerror(errno));
	ccb.cgdl.periph_name[0] = '\0';
	errno = 0;
	CHECK(ioctl(fd, CAMGETPASSTHRU, &ccb) == -1 && errno == EINVAL,
	    "GETPASSTHRU with no name: %s", strerror(errno));

	/* Each disk's peripheral leads to its pass device. */
	for (i = 0; i < n; i++) {
		if (all[i].type != DEV_MATCH_PERIPH)
			continue;
		pr = &all[i].result.periph_result;
		if (strcmp(pr->periph_name, "pass") == 0) {
			check_pass(pr->unit_number);
			continue;
		}
		if (strcmp(pr->periph_name, "xpt") == 0)
			continue;
		memset(&ccb, 0, sizeof(ccb));
		strlcpy(ccb.cgdl.periph_name, pr->periph_name,
		    sizeof(ccb.cgdl.periph_name));
		ccb.cgdl.unit_number = pr->unit_number;
		CHECK(ioctl(fd, CAMGETPASSTHRU, &ccb) == 0 &&
		    strcmp(ccb.cgdl.periph_name, "pass") == 0 &&
		    ccb.ccb_h.path_id == pr->path_id &&
		    ccb.ccb_h.target_lun == pr->target_lun,
		    "GETPASSTHRU %s%u: %s", pr->periph_name, pr->unit_number,
		    strerror(errno));
	}
	close(fd);

	check_nvme_nodes();
	return (failed);
}
