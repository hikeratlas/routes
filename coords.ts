// Adapted from https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames#ECMAScript_(JavaScript/ActionScript,_etc.)
export const lon2x = (lon: number, zoom: number) => Math.floor((lon + 180) / 360 * Math.pow(2, zoom));
export const lat2y = (lat: number, zoom: number) => Math.floor((1 - Math.log(Math.tan(lat * Math.PI / 180) + 1 / Math.cos(lat * Math.PI / 180)) / Math.PI) / 2 * Math.pow(2 ,zoom));

export const x2lon = (x: number, z: number) => x / Math.pow(2, z) * 360 - 180;
export const y2lat = (y: number, z: number) => {
	const n = Math.PI - 2 * Math.PI * y / Math.pow(2, z);
	return 180 / Math.PI * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n)));
}
