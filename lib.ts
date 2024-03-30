import type { Feature, NodeMap, Tile } from './types';
import { BboxBitmap } from './bbox-bitmap';
import { highestCommonTile, highestCommonTileForGeometry } from './coords';

// TODO: why can't we import this? get some weird type error.
const pointInPolygon = require('@turf/boolean-point-in-polygon').default;
const fs = require('node:fs');

export const load = (filename: string): Feature[] => {
	const lines = fs.readFileSync(filename, 'utf-8')
		.split('\n')
		.filter((x: string) => x)
		.map((x: string) => JSON.parse(x));
	//console.log(lines);
//	console.log(lines.length);
	return lines;
}

export const ll = (coord: [number, number]): string => coord.join(',');

export const tile2str = (tile: Tile): string => `z${tile.z}/${tile.x}/${tile.y}`;

export const buildIndex = (features: Feature[]): NodeMap => {
	const map: NodeMap = {};

	const parkingLotTiles: { [key: string]: Feature[] } = {};

	const index = (properties: Feature['properties'], coordinates: [number, number], neighbour?: [number, number]) => {
		const key = ll(coordinates);

		let v = map[key];
		if (!v) {
			v = map[key] = {
				ll: coordinates,
				nodes: [],
				edges: [],
				inParkingLot: false
			};
		}

		// Ways often contain the same node twice; don't add it twice.
		let found = false;
		for (const node of v.nodes)
			if (node === properties) {
				found = true;
				break;
			}

		if (!found) {
				v.nodes.push(properties);

			if (neighbour)
				v.edges.push(neighbour);

			if (isParkingLot(properties))
				v.inParkingLot = true;
		}
	};

	const parkingLotBitmap = BboxBitmap();
	const parkingLots: Feature[] = [];

	let start = Date.now();
	for (const feature of features) {
		if (isParkingLot(feature.properties)) {
			const tile = highestCommonTileForGeometry(feature.geometry);
			const tileKey = tile2str(tile);
			parkingLotTiles[tileKey] = parkingLotTiles[tileKey] || [];
			parkingLotTiles[tileKey].push(feature);
			parkingLots.push(feature);

			if (feature.geometry.type === 'MultiPolygon')
				parkingLotBitmap.add(feature);
		}

		if (feature.geometry.type === 'Point') {
			index(feature.properties, feature.geometry.coordinates);
		} else if (feature.geometry.type === 'LineString') {
			const coords = feature.geometry.coordinates;
			for (let i = 0; i < coords.length; i++)
				index(feature.properties, coords[i], coords[i + 1]);
		} else if (feature.geometry.type === 'MultiLineString') {
		} else if (feature.geometry.type === 'MultiPolygon') {
			// Just index the outer rings, e.g. for parking lots
			for (const poly of feature.geometry.coordinates) {
				const coords = poly[0];
				// The last coord is the same as the first coord, so skip it.
				for (let i = 0; i < coords.length - 1; i++)
					index(feature.properties, coords[i], coords[i + 1]);
			}
		}
	}
	//console.log(`pass 1 took ${Date.now() - start} ms`);
	start = Date.now();

	// CONSIDER: This could be parallelized, it's ~80% of the
	// runtime.
	for (const v of Object.values(map)) {
		if (!parkingLotBitmap.test(v.ll))
			continue;

		let needle = highestCommonTile([v.ll]);

		const overlappingParkingLots = [];
		while(needle.z > 0) {
			const key = tile2str(needle);

			const toAdd = parkingLotTiles[key];
			if (toAdd)
				overlappingParkingLots.push(...toAdd);

			needle.z--;
			needle.x = Math.floor(needle.x / 2);
			needle.y = Math.floor(needle.y / 2);
		}

		for (const feature of overlappingParkingLots) {
			if (feature.geometry.type !== 'MultiPolygon')
				continue;

			if (pointInPolygon(v.ll, feature))
				v.inParkingLot = true;
		}
	}
	//console.log(`pass 2 took ${Date.now() - start} ms`);

	return map;
}

export const isParkingLot = (node: Feature['properties']): boolean => {
	return node['amenity'] === 'parking';
}

export const isFootPath = (node: Feature['properties']): boolean => {
	return node['highway'] === 'footway' || // Mizzy Lake Trail
		node['highway'] === 'path' // Bat Lake Trail;
}

export const isRoad = (node: Feature['properties']): boolean => {
	return node['highway'] === 'motorway' ||
		node['highway'] === 'trunk' ||
		node['highway'] === 'primary' ||
		node['highway'] === 'secondary' ||
		node['highway'] === 'tertiary' ||
		node['highway'] === 'unclassified' ||
		node['highway'] === 'residential' ||
		node['highway'] === 'service';

	// Consider: service, track, road
}

export const getTrailheads = (map: NodeMap): string[] => {
	const rv: string[] = [];

	for (const [ll, v] of Object.entries(map)) {
		const paths = v.nodes.find(isFootPath);
		if (!paths)
			continue;

		const roads = v.nodes.find(isRoad);
		if (v.inParkingLot || roads) {
			rv.push(ll);
		}
	}
	return rv;
};
