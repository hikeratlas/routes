import type { Feature, Geometry, Tile } from './types';

// Adapted from https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames#ECMAScript_(JavaScript/ActionScript,_etc.)
export const lon2x = (lon: number, zoom: number) => Math.floor((lon + 180) / 360 * Math.pow(2, zoom));
export const lat2y = (lat: number, zoom: number) => Math.floor((1 - Math.log(Math.tan(lat * Math.PI / 180) + 1 / Math.cos(lat * Math.PI / 180)) / Math.PI) / 2 * Math.pow(2 ,zoom));

export const x2lon = (x: number, z: number) => x / Math.pow(2, z) * 360 - 180;
export const y2lat = (y: number, z: number) => {
	const n = Math.PI - 2 * Math.PI * y / Math.pow(2, z);
	return 180 / Math.PI * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n)));
}

export const commonParent = (a: Tile, b: Tile): Tile => {
	if (a.z === b.z && a.x === b.x && a.y === b.y)
		return a;

	if (a.z === b.z) {
		return commonParent(
			{ z: a.z - 1, x: Math.floor(a.x / 2), y: Math.floor(a.y / 2) },
			{ z: b.z - 1, x: Math.floor(b.x / 2), y: Math.floor(b.y / 2) },
		);
	};

	if (a.z > b.z) {
		return commonParent(
			{ z: a.z - 1, x: Math.floor(a.x / 2), y: Math.floor(a.y / 2) },
			b
		);
	}

	if (a.z < b.z) {
		return commonParent(
			a,
			{ z: b.z - 1, x: Math.floor(b.x / 2), y: Math.floor(b.y / 2) }
		);
	}

	throw new Error('unhandled');
};

export const highestCommonTile = (lls: [number, number][]): Tile => {
	if (lls.length === 0)
		throw new Error(`lls must not be empty`);

	let minLon = 999, minLat = 999, maxLon = -999, maxLat = -999;

	for (const [lon, lat] of lls) {
		if (lon > maxLon) maxLon = lon;
		if (lon < minLon) minLon = lon;
		if (lat > maxLat) maxLat = lat;
		if (lat < minLat) minLat = lat;
	}

	const z = 18;
	const a = { z, x: lon2x(minLon, z), y: lat2y(minLat, z) };
	const b = { z, x: lon2x(maxLon, z), y: lat2y(maxLat, z) };
	return commonParent(a, b);
}

export const highestCommonTileForGeometry = (geom: Geometry): Tile => {
	if (geom.type === 'Point')
		return highestCommonTile([geom.coordinates]);

	if (geom.type === 'LineString')
		return highestCommonTile(geom.coordinates);

	if (geom.type === 'MultiLineString') {
		const lls: [number, number][] = [];
		for (const ls of geom.coordinates)
			lls.push(...ls);

		return highestCommonTile(lls);
	}

	if (geom.type === 'MultiPolygon') {
		const lls: [number, number][] = [];
		for (const ls of geom.coordinates)
			lls.push(...ls[0]);

		return highestCommonTile(lls);
	}

	throw new Error(`unexpected geometry: ${JSON.stringify(geom)}`);
}

export const highestCommonTileForFeature = (feature: Feature): Tile => {
	return highestCommonTileForGeometry(feature.geometry);
}
