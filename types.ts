export interface Point {
	type: 'Point';
	coordinates: [number, number];
};

export interface LineString {
	type: 'LineString';
	coordinates: [number, number][];
};

export interface MultiLineString {
	type: 'MultiLineString';
	coordinates: [number, number][][];
};

export interface MultiPolygon {
	type: 'MultiPolygon';
	coordinates: [number, number][][][];
};

export type Geometry = Point | LineString | MultiLineString | MultiPolygon;

export interface Feature {
	type: 'Feature';
	geometry: Geometry;
	// We could consider fleshing these out...
	properties: { [k: string]: string };
}

export type NodeMap = {
	[ll: string]: {
		// The lon/lat of this node
		ll: [number, number];
		// What's here -- parking lot, hiking trail, etc.
		nodes: Feature['properties'][];
		// The places that are reachable from here.
		edges: [number, number][];

		inParkingLot: boolean;
	}
};
